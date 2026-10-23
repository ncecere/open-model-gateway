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
    inference::types::ChatRequest,
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

    /// Whether *every* authorized, protocol-capable candidate deployment of
    /// `model` would deterministically refuse `request` on `protocol`: the
    /// same per-adapter/provider enumeration `Engine::execute_protocol` runs
    /// before interactive admission (#228), reused here read-only so an
    /// unpinned batch line's exclusion from the parent envelope matches what
    /// the runner's own dispatch would actually do (#238) - not just what
    /// the one deployment planning happened to size the line against.
    ///
    /// `Ok(None)`: at least one capable candidate accepts, or none was even
    /// protocol-capable for this request (the coarse whole-batch check
    /// already reports that case). `Ok(Some(error))`: every capable
    /// candidate refuses, classified like the engine would (400
    /// `invalid_request_error` only when a route the live routing policy
    /// would actually use returned it, else 501 `unsupported_capability`).
    /// No admission, reservation or routing-health write happens here.
    async fn every_candidate_route_refuses(
        &self,
        principal: &Principal,
        model: &str,
        protocol: ApiProtocol,
        request_id: Uuid,
        request: &ChatRequest,
    ) -> Result<Option<InferenceError>, JobError> {
        let deployments = self.store.deployments(principal, model).await?;
        let mut candidates: Vec<Deployment> = Vec::new();
        let mut refused: Vec<(Deployment, InferenceError)> = Vec::new();
        let mut verdicts: HashMap<String, Result<(), InferenceError>> = HashMap::new();
        for deployment in deployments {
            if !deployment
                .supported_protocols
                .iter()
                .any(|p| p == protocol.as_str())
            {
                continue;
            }
            let Some(adapter) = self.registry.get(&deployment.provider) else {
                continue;
            };
            if !(adapter.supports_protocol(protocol) && adapter.supports_chat_request(request)) {
                continue;
            }
            let verdict = *verdicts
                .entry(deployment.provider.clone())
                .or_insert_with(|| match adapter.validate_chat_request(request) {
                    Ok(()) => Ok(()),
                    Err(InferenceError::InvalidRequest) => Err(InferenceError::InvalidRequest),
                    Err(InferenceError::Unsupported) => Err(InferenceError::Unsupported),
                    // Outside the adapter's contract (#228): never silently
                    // admitted, logged for the operator, same as the engine.
                    Err(other) => {
                        tracing::warn!(provider = %deployment.provider, error = ?other,
                            "adapter request validation returned an error outside its contract");
                        Err(InferenceError::Unsupported)
                    }
                });
            match verdict {
                Ok(()) => candidates.push(deployment),
                Err(error) => refused.push((deployment, error)),
            }
        }
        if !candidates.is_empty() || refused.is_empty() {
            return Ok(None);
        }
        // Every protocol-capable candidate refused: classify it only from
        // the routes the read-only routing policy would actually use
        // (residency, operator-disabled routes and cooldown), exactly as
        // `Engine::execute_protocol` does for an interactive request.
        let refused_deployments: Vec<Deployment> = refused.iter().map(|(d, _)| d.clone()).collect();
        let usable = self
            .store
            .route_plan(principal, model, &refused_deployments, request_id)
            .await?;
        let invalid = refused.iter().any(|(d, error)| {
            *error == InferenceError::InvalidRequest && usable.deployment_ids.contains(&d.id)
        });
        Ok(Some(if invalid {
            InferenceError::InvalidRequest
        } else {
            InferenceError::Unsupported
        }))
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
        let mut previews: HashMap<(Uuid, Option<u32>), LinePreview> = HashMap::new();
        let mut prices: HashMap<Uuid, Uuid> = HashMap::new();
        // deployment id -> (group, first line number using it)
        let mut groups: Vec<(LineGroup, u64, Arc<dyn ProviderAdapter>, bool)> = Vec::new();
        let mut requests: u32 = 0;
        let mut physical: u64 = 0;
        // Lines a native submission could not encode / the engine cannot run.
        let mut native_ok = true;
        let mut gateway_unsupported: Option<u64> = None;
        // Lines the coarse check passed but the adapter's own pure encoder
        // (`ProviderAdapter::validate_chat_request`, #228) refuses
        // deterministically: excluded from the parent envelope, not from the
        // batch (see the end of the scan).
        let mut deterministic_rejects: Vec<(u64, InferenceError)> = Vec::new();
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
            let requested = match &line.request {
                BatchRequest::Chat(r) => r.max_output_tokens,
                BatchRequest::Embeddings(_) => Some(0),
            };
            let key = (resolved.deployment.id, requested);
            if let std::collections::hash_map::Entry::Vacant(slot) = previews.entry(key) {
                slot.insert(
                    crate::governance::batch::preview_line(
                        &self.store,
                        resolved.deployment.id,
                        endpoint.protocol(),
                        requested,
                        prices.get(&resolved.deployment.id).copied(),
                        resolved.adapter.generation_output_limit(),
                    )
                    .await?,
                );
            }
            let (price_id, max) = match previews[&key] {
                LinePreview::Ok { price_id, output } => (price_id, output),
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
            };
            prices.insert(resolved.deployment.id, price_id);
            // Encoding checks use the same bound as the parent hold; the
            // uploaded body and private body retain the optional omission.
            let mut execution_request = line.request.clone();
            if let BatchRequest::Chat(r) = &mut execution_request {
                r.max_output_tokens = Some(max);
            }
            // Can this line run natively, and can the engine run it?
            let encodable = resolved
                .adapter
                .native_batch(&resolved.deployment, endpoint)
                && resolved
                    .adapter
                    .encode_native_line(&resolved.deployment, endpoint, "l0", &execution_request)
                    .is_ok();
            if resolved.legacy && !encodable {
                if issue(&mut issues, physical, LineError::UnsupportedFeature) {
                    break;
                }
                continue;
            }
            native_ok &= encodable;
            let coarse_ok = resolved.legacy
                || match &line.request {
                    BatchRequest::Chat(r) => resolved.adapter.supports_chat_request(r),
                    BatchRequest::Embeddings(r) => resolved
                        .adapter
                        .supports_embedding_target(&resolved.deployment, r),
                };
            if !coarse_ok && gateway_unsupported.is_none() {
                gateway_unsupported = Some(physical);
            }
            // The coarse bool can pass while an adapter's own pure encoder
            // still refuses this exact body (a strict function tool, an
            // out-of-range sampling parameter, a tool result with no
            // matching call, ...): the same deterministic check
            // `Engine::execute_protocol` runs before admission (#228). A
            // model set up only for native batches (`resolved.legacy`) never
            // reaches the generic engine, so it is exempt.
            //
            // An omitted maximum pins this line to `resolved.deployment` for
            // both sizing and dispatch (`lines::LinePlan`, below): judge it
            // on that route alone. An explicit maximum is copied with no pin;
            // the runner may schedule it onto any authorized,
            // protocol-capable candidate route for the model at dispatch
            // (`jobs::runner::Runner::plan_route_inner`), so excluding it
            // from the envelope here is correct only when *every* such
            // candidate would refuse it too - the same per-adapter/provider
            // enumeration `Engine::execute_protocol` runs, reused here
            // read-only (no admission, no routing-health writes).
            let deterministic_reject = if resolved.legacy {
                None
            } else {
                match &execution_request {
                    BatchRequest::Chat(r) => {
                        if requested.is_none() {
                            coarse_ok
                                .then(|| resolved.adapter.validate_chat_request(r).err())
                                .flatten()
                        } else {
                            self.every_candidate_route_refuses(
                                principal,
                                &model,
                                endpoint.protocol(),
                                request_id,
                                r,
                            )
                            .await?
                        }
                    }
                    BatchRequest::Embeddings(_) => None,
                }
            };
            if !issues.is_empty() {
                // Keep scanning for problems, but stop copying.
                continue;
            }
            if let Some(error) = deterministic_reject {
                // The engine refuses this line before it ever admits or
                // reserves anything for it (zero cost, #228): it must not
                // size or reserve the parent envelope either. It is still
                // copied below, so it still runs and is refused per line, at
                // the same classification as an interactive request, when
                // the batch dispatches (`jobs::runner`).
                deterministic_rejects.push((physical, error));
            } else {
                match groups
                    .iter_mut()
                    .find(|g| g.0.deployment.id == resolved.deployment.id)
                {
                    Some(g) => {
                        *g.0.outputs.entry(max).or_insert(0) += 1;
                        if requested.is_none() {
                            g.0.price_id = Some(price_id);
                        }
                    }
                    None => groups.push((
                        LineGroup {
                            deployment: resolved.deployment.clone(),
                            model: model.clone(),
                            protocol: endpoint.protocol(),
                            outputs: BTreeMap::from([(max, 1)]),
                            price_id: requested.is_none().then_some(price_id),
                        },
                        physical,
                        resolved.adapter.clone(),
                        resolved.legacy,
                    )),
                }
            }
            let mut copy = if requested.is_none() {
                lines::planned_copy(
                    raw,
                    lines::LinePlan {
                        deployment: resolved.deployment.id,
                        price: price_id,
                    },
                )?
            } else {
                raw
            };
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
            && deterministic_rejects.is_empty()
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
        if issues.is_empty() && groups.is_empty() {
            // Every candidate line was a deterministic adapter rejection:
            // there is nothing to reserve or dispatch. Refuse the upload
            // with a line-numbered report, like any other invalid line,
            // instead of admitting an empty batch.
            for (line, error) in deterministic_rejects.iter().copied() {
                let error = match error {
                    InferenceError::InvalidRequest => LineError::InvalidBody,
                    InferenceError::Unsupported => LineError::UnsupportedFeature,
                    other => {
                        tracing::warn!(error = ?other, "adapter request validation returned an error outside its contract");
                        LineError::UnsupportedFeature
                    }
                };
                if issue(&mut issues, line, error) {
                    break;
                }
            }
            if issues.is_empty() {
                issues.push(Issue {
                    line: 1,
                    error: LineError::EmptyFile,
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
