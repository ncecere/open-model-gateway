//! Batch validation and planning at create time. The input file is streamed
//! once: every line is checked (shape, unique `custom_id`, the endpoint's body
//! contract, model available to this key, priced and bounded), valid lines
//! are copied to the batch's private file (internal, never listed; the client may
//! delete its input later), and the lines are grouped by deployment for the
//! batch's ceiling. Invalid files are rejected with a line-numbered report.
use std::collections::{BTreeMap, HashMap, HashSet};

use super::{
    io::{LazySink, LineReader},
    lines::{LineError, parse_line},
    *,
};
use crate::{
    filestore::StoredFile,
    governance::batch::{LineGroup, LinePreview},
};

/// At most this many problems are reported (scanning stops there).
pub const MAX_ISSUES: usize = 20;

/// One reported problem: the 1-based physical line number of the file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Issue {
    pub line: u64,
    pub error: LineError,
}

pub(crate) enum PlanError {
    Invalid(Vec<Issue>),
    Job(JobError),
}
impl From<JobError> for PlanError {
    fn from(e: JobError) -> Self {
        Self::Job(e)
    }
}
impl From<InferenceError> for PlanError {
    fn from(e: InferenceError) -> Self {
        Self::Job(e.into())
    }
}

/// The validated batch.
pub(crate) struct Plan {
    pub groups: Vec<LineGroup>,
    pub requests: u32,
    /// Set when the batch runs natively (one deployment, adapter support).
    pub native: Option<(Deployment, Arc<dyn ProviderAdapter>)>,
    pub work_file: Uuid,
    /// The model label (one model, or `mixed`).
    pub model: String,
}

struct Resolved {
    deployment: Deployment,
    adapter: Arc<dyn ProviderAdapter>,
    /// A model set up as "Batch" (0016): native Chat Completions only.
    legacy: bool,
}

impl Jobs {
    async fn resolve(
        &self,
        principal: &Principal,
        model: &str,
        endpoint: BatchEndpoint,
        request_id: Uuid,
    ) -> Result<Result<Resolved, LineError>, JobError> {
        let picked = self
            .select(principal, model, endpoint.protocol(), request_id, |_, _| {
                true
            })
            .await;
        let picked = match picked {
            Err(InferenceError::Unsupported) if endpoint == BatchEndpoint::ChatCompletions => self
                .select(
                    principal,
                    model,
                    ApiProtocol::Batches,
                    request_id,
                    |a, d| a.native_batch(d, endpoint),
                )
                .await
                .map(|(d, a)| (d, a, true)),
            other => other.map(|(d, a)| (d, a, false)),
        };
        Ok(match picked {
            Ok((deployment, adapter, legacy)) => Ok(Resolved {
                deployment,
                adapter,
                legacy,
            }),
            Err(InferenceError::ModelUnavailable) => Err(LineError::ModelNotFound),
            Err(InferenceError::Unsupported) => Err(LineError::ModelUnsupported),
            Err(e) => return Err(e.into()),
        })
    }

    /// Validate `input` (already opened) and copy it to the batch's private
    /// file. See the module docs.
    pub(crate) async fn plan(
        &self,
        principal: &Principal,
        request_id: Uuid,
        input: &StoredFile,
        endpoint: BatchEndpoint,
        force_gateway: bool,
    ) -> Result<Plan, PlanError> {
        let files = self.files()?;
        let mut reader = LineReader::stored(
            files,
            input.id,
            principal.workspace_id,
            BATCH_MAX_LINE_BYTES,
        )
        .await
        .map_err(|_| JobError::NotFound)?;
        let mut work = LazySink::new(
            files,
            io::batch_file(
                true,
                principal.workspace_id,
                principal.key_id,
                principal.user_id,
                format!("batch_{}_input.jsonl", request_id.simple()),
            ),
        );
        let result = self
            .scan(
                principal,
                request_id,
                endpoint,
                force_gateway,
                &mut reader,
                &mut work,
            )
            .await;
        match result {
            Ok((groups, requests, native, model)) => {
                let work_file = work.finish().await?.ok_or(PlanError::Invalid(vec![Issue {
                    line: 1,
                    error: LineError::EmptyFile,
                }]))?;
                Ok(Plan {
                    groups,
                    requests,
                    native,
                    work_file,
                    model,
                })
            }
            Err(e) => {
                work.abort().await;
                Err(e)
            }
        }
    }

    #[allow(clippy::type_complexity)]
    async fn scan(
        &self,
        principal: &Principal,
        request_id: Uuid,
        endpoint: BatchEndpoint,
        force_gateway: bool,
        reader: &mut LineReader,
        work: &mut LazySink,
    ) -> Result<
        (
            Vec<LineGroup>,
            u32,
            Option<(Deployment, Arc<dyn ProviderAdapter>)>,
            String,
        ),
        PlanError,
    > {
        let mut issues: Vec<Issue> = Vec::new();
        let mut ids: HashSet<String> = HashSet::new();
        let mut models: HashMap<String, Result<Resolved, LineError>> = HashMap::new();
        let mut previews: HashMap<(Uuid, u32), LinePreview> = HashMap::new();
        // deployment id -> (group, first line number using it)
        let mut groups: Vec<(LineGroup, u64, Arc<dyn ProviderAdapter>, bool)> = Vec::new();
        let mut requests: u32 = 0;
        let mut physical: u64 = 0;
        // Lines a native submission could not encode / the engine cannot run.
        let mut native_ok = true;
        let mut gateway_unsupported: Option<u64> = None;
        let issue = |issues: &mut Vec<Issue>, line: u64, error: LineError| {
            issues.push(Issue { line, error });
            issues.len() >= MAX_ISSUES
        };
        loop {
            let raw = match reader.next().await {
                Ok(Some(raw)) => raw,
                Ok(None) => break,
                Err(InferenceError::InvalidUpstream) => {
                    issue(&mut issues, physical + 1, LineError::LineTooLarge);
                    break;
                }
                Err(e) => return Err(e.into()),
            };
            physical += 1;
            if lines::is_blank(&raw) {
                continue;
            }
            if requests >= BATCH_MAX_REQUESTS {
                issue(&mut issues, physical, LineError::TooManyLines);
                break;
            }
            requests += 1;
            let line = match parse_line(&raw, endpoint) {
                Ok(l) => l,
                Err(e) => {
                    if issue(&mut issues, physical, e) {
                        break;
                    }
                    continue;
                }
            };
            if !ids.insert(line.custom_id.clone()) {
                if issue(&mut issues, physical, LineError::DuplicateCustomId) {
                    break;
                }
                continue;
            }
            let model = line.request.model().to_owned();
            if let std::collections::hash_map::Entry::Vacant(slot) = models.entry(model.clone()) {
                slot.insert(
                    self.resolve(principal, &model, endpoint, request_id)
                        .await?,
                );
            }
            let resolved = match &models[&model] {
                Ok(r) => r,
                Err(e) => {
                    if issue(&mut issues, physical, *e) {
                        break;
                    }
                    continue;
                }
            };
            let max = line.request.max_output();
            let key = (resolved.deployment.id, max);
            if let std::collections::hash_map::Entry::Vacant(slot) = previews.entry(key) {
                slot.insert(
                    crate::governance::batch::preview_line(
                        &self.store,
                        resolved.deployment.id,
                        endpoint.protocol(),
                        max,
                    )
                    .await?,
                );
            }
            match previews[&key] {
                LinePreview::Ok => {}
                LinePreview::OutputLimit => {
                    if issue(&mut issues, physical, LineError::OutputLimit) {
                        break;
                    }
                    continue;
                }
                LinePreview::Unpriced | LinePreview::Unbounded => {
                    if issue(&mut issues, physical, LineError::Unpriced) {
                        break;
                    }
                    continue;
                }
            }
            // Can this line run natively, and can the engine run it?
            let encodable = resolved
                .adapter
                .native_batch(&resolved.deployment, endpoint)
                && resolved
                    .adapter
                    .encode_native_line(&resolved.deployment, endpoint, "l0", &line.request)
                    .is_ok();
            if resolved.legacy && !encodable {
                if issue(&mut issues, physical, LineError::UnsupportedFeature) {
                    break;
                }
                continue;
            }
            native_ok &= encodable;
            let runnable = resolved.legacy
                || match &line.request {
                    BatchRequest::Chat(r) => resolved.adapter.supports_chat_request(r),
                    BatchRequest::Embeddings(r) => resolved
                        .adapter
                        .supports_embedding_target(&resolved.deployment, r),
                };
            if !runnable && gateway_unsupported.is_none() {
                gateway_unsupported = Some(physical);
            }
            if !issues.is_empty() {
                // Keep scanning for problems, but stop copying.
                continue;
            }
            match groups
                .iter_mut()
                .find(|g| g.0.deployment.id == resolved.deployment.id)
            {
                Some(g) => *g.0.outputs.entry(max).or_insert(0) += 1,
                None => groups.push((
                    LineGroup {
                        deployment: resolved.deployment.clone(),
                        model: model.clone(),
                        protocol: endpoint.protocol(),
                        outputs: BTreeMap::from([(max, 1)]),
                    },
                    physical,
                    resolved.adapter.clone(),
                    resolved.legacy,
                )),
            }
            let mut copy = raw;
            copy.push(b'\n');
            work.raw(copy).await?;
        }
        if requests == 0 && issues.is_empty() {
            issues.push(Issue {
                line: 1,
                error: LineError::EmptyFile,
            });
        }
        let native = !force_gateway
            && native_ok
            && groups.len() == 1
            && groups[0].2.native_batch(&groups[0].0.deployment, endpoint);
        if issues.is_empty() && !native {
            // Lines only a native batch could run.
            if let Some(g) = groups.iter().find(|g| g.3) {
                issues.push(Issue {
                    line: g.1,
                    error: LineError::ModelUnsupported,
                });
            } else if let Some(line) = gateway_unsupported {
                issues.push(Issue {
                    line,
                    error: LineError::UnsupportedFeature,
                });
            }
        }
        if !issues.is_empty() {
            return Err(PlanError::Invalid(issues));
        }
        let model = if groups.len() == 1 {
            groups[0].0.model.clone()
        } else {
            "mixed".to_owned()
        };
        let native = native.then(|| (groups[0].0.deployment.clone(), groups[0].2.clone()));
        Ok((
            groups.into_iter().map(|g| g.0).collect(),
            requests,
            native,
            model,
        ))
    }
}
