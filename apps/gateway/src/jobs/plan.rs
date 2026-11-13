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

mod scan;

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

/// Test-only counters proving [`Jobs::every_candidate_route_refuses`]'s two
/// model-level, read-only lookups (`Store::deployments`, `Store::route_plan`)
/// are memoized at most once per model per scan (#238 follow-up), not once
/// per explicit-maximum line. Incremented only at those two call sites;
/// never read outside tests.
#[cfg(test)]
#[derive(Default)]
pub(crate) struct PlanLookupCounts {
    pub(crate) deployments: std::sync::atomic::AtomicUsize,
    pub(crate) route_plan: std::sync::atomic::AtomicUsize,
}

/// Per-scan memoization of [`Jobs::every_candidate_route_refuses`]'s
/// model-level, read-only lookups. With configuration caches bypassed
/// (`GATEWAY_CONFIG_CACHE=off`, or a version-unconfirmed cache miss), a
/// naive per-line call ran `Store::deployments` and (when every capable
/// route refused) `Store::route_plan` for every explicit-maximum line, so a
/// large upload amplified database load before `admit_batch` ever checked a
/// budget or job limit. Neither entry survives this one scan: nothing here
/// is cached across scans or requests.
#[derive(Default)]
struct RefusalMemo {
    /// A model's candidate deployments (`Store::deployments`) never depend
    /// on a line's body: fetched at most once per model per scan.
    deployments: HashMap<String, Arc<Vec<Deployment>>>,
    /// The routes the read-only routing policy would use (`route_plan`)
    /// for a given set of refused candidates, keyed by model and the sorted
    /// refused deployment ids. Which routes are capable depends on the line
    /// (coarse feature support), and the refusal *kind* depends on the
    /// line's body, so only this usable-route set is reused; the 400/501
    /// decision is still made per line from its own adapter verdicts.
    usable: HashMap<(String, Vec<Uuid>), Arc<Vec<Uuid>>>,
}

/// What [`Jobs::scan`] returns: the groups, the request count, the native
/// route (when the batch runs natively) and the model label.
type Scanned = (
    Vec<LineGroup>,
    u32,
    Option<(Deployment, Arc<dyn ProviderAdapter>)>,
    String,
);

/// The per-scan arguments every line step needs.
#[derive(Clone, Copy)]
struct ScanScope<'a> {
    principal: &'a Principal,
    request_id: Uuid,
    endpoint: BatchEndpoint,
}

/// A deployment's group, built while scanning.
struct PendingGroup {
    group: LineGroup,
    /// The first line number using the deployment.
    first_line: u64,
    adapter: Arc<dyn ProviderAdapter>,
    legacy: bool,
}

/// The scan's mutable state.
struct ScanState {
    issues: Vec<Issue>,
    ids: HashSet<String>,
    models: HashMap<String, Result<Arc<Resolved>, LineError>>,
    // Cache TTL requirements are part of the memoization key: a clean
    // line must not authorize a later cached line at an incompatible price.
    previews: HashMap<(Uuid, Option<u32>, u8), LinePreview>,
    prices: HashMap<Uuid, Uuid>,
    // deployment id -> (group, first line number using it)
    groups: Vec<PendingGroup>,
    requests: u32,
    physical: u64,
    // Lines a native submission could not encode / the engine cannot run.
    native_ok: bool,
    gateway_unsupported: Option<u64>,
    // Lines the coarse check passed but the adapter's own pure encoder
    // (`ProviderAdapter::validate_chat_request`, #228) refuses
    // deterministically: excluded from the parent envelope, not from the
    // batch (see the end of the scan).
    deterministic_rejects: Vec<(u64, InferenceError)>,
    // Model-level memoization of `every_candidate_route_refuses`'s
    // read-only lookups, scoped to this one scan (#238 follow-up).
    refusal_memo: RefusalMemo,
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
    ///
    /// Both DB-backed lookups this needs are memoized in `memo` for the
    /// whole scan (#238 follow-up): `Store::deployments` once per model, and
    /// (only when every capable route refuses) `Store::route_plan` once per
    /// model and refused-route set. The refusal kind stays per line:
    /// a 50,000-line upload must not run ~50k deployment reads and ~100k
    /// routing selects before `admit_batch` ever checks a budget or job
    /// limit. Per-line adapter validation (bodies differ) still runs on
    /// every line, deduplicated per provider within that line as before;
    /// nothing here survives past this one scan.
    async fn every_candidate_route_refuses(
        &self,
        principal: &Principal,
        model: &str,
        protocol: ApiProtocol,
        request_id: Uuid,
        request: &ChatRequest,
        memo: &mut RefusalMemo,
    ) -> Result<Option<InferenceError>, JobError> {
        let deployments = match memo.deployments.get(model) {
            Some(cached) => cached.clone(),
            None => {
                #[cfg(test)]
                self.plan_lookup_counts
                    .deployments
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let fetched = Arc::new(self.store.deployments(principal, model).await?);
                memo.deployments.insert(model.to_owned(), fetched.clone());
                fetched
            }
        };
        let mut candidates: Vec<Deployment> = Vec::new();
        let mut refused: Vec<(Deployment, InferenceError)> = Vec::new();
        let mut verdicts: HashMap<String, Result<(), InferenceError>> = HashMap::new();
        for deployment in deployments.iter() {
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
                    Err(e @ (InferenceError::InvalidRequest | InferenceError::InvalidField(_))) => {
                        Err(e)
                    }
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
                Ok(()) => candidates.push(deployment.clone()),
                Err(error) => refused.push((deployment.clone(), error)),
            }
        }
        if !candidates.is_empty() || refused.is_empty() {
            return Ok(None);
        }
        // Every protocol-capable candidate refused: classify it only from
        // the routes the read-only routing policy would actually use
        // (residency, operator-disabled routes and cooldown), exactly as
        // `Engine::execute_protocol` does for an interactive request. The
        // usable-route set is memoized per (model, refused set); the kind
        // below is still decided from this line's own verdicts.
        let mut key_ids: Vec<Uuid> = refused.iter().map(|(d, _)| d.id).collect();
        key_ids.sort_unstable();
        let key = (model.to_owned(), key_ids);
        let usable = match memo.usable.get(&key) {
            Some(cached) => cached.clone(),
            None => {
                #[cfg(test)]
                self.plan_lookup_counts
                    .route_plan
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let refused_deployments: Vec<Deployment> =
                    refused.iter().map(|(d, _)| d.clone()).collect();
                let plan = self
                    .store
                    .route_plan(principal, model, &refused_deployments, request_id)
                    .await?;
                let ids = Arc::new(plan.deployment_ids);
                memo.usable.insert(key, ids.clone());
                ids
            }
        };
        let invalid = refused.iter().find(|(d, error)| {
            matches!(
                error,
                InferenceError::InvalidRequest | InferenceError::InvalidField(_)
            ) && usable.contains(&d.id)
        });
        Ok(Some(
            invalid.map_or(InferenceError::Unsupported, |(_, e)| *e),
        ))
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

    async fn scan(
        &self,
        principal: &Principal,
        request_id: Uuid,
        endpoint: BatchEndpoint,
        force_gateway: bool,
        reader: &mut LineReader,
        work: &mut LazySink,
    ) -> Result<Scanned, PlanError> {
        let scope = ScanScope {
            principal,
            request_id,
            endpoint,
        };
        let mut state = ScanState::new();
        loop {
            let raw = match reader.next().await {
                Ok(Some(raw)) => raw,
                Ok(None) => break,
                Err(InferenceError::InvalidUpstream) => {
                    state.issue(state.physical + 1, LineError::LineTooLarge);
                    break;
                }
                Err(e) => return Err(e.into()),
            };
            state.physical += 1;
            if lines::is_blank(&raw) {
                continue;
            }
            if state.requests >= BATCH_MAX_REQUESTS {
                state.issue(state.physical, LineError::TooManyLines);
                break;
            }
            state.requests += 1;
            let line = match self.check_line(scope, &mut state, &raw).await? {
                Ok(line) => line,
                Err(e) => {
                    if state.issue(state.physical, e) {
                        break;
                    }
                    continue;
                }
            };
            if !state.issues.is_empty() {
                // Keep scanning for problems, but stop copying.
                continue;
            }
            state.record(endpoint, &line);
            scan::copy_line(work, raw, &line).await?;
        }
        state.finish(endpoint, force_gateway)
    }
}

impl ScanState {
    fn new() -> Self {
        Self {
            issues: Vec::new(),
            ids: HashSet::new(),
            models: HashMap::new(),
            previews: HashMap::new(),
            prices: HashMap::new(),
            groups: Vec::new(),
            requests: 0,
            physical: 0,
            native_ok: true,
            gateway_unsupported: None,
            deterministic_rejects: Vec::new(),
            refusal_memo: RefusalMemo::default(),
        }
    }

    /// Report a problem; `true` once [`MAX_ISSUES`] are reported.
    fn issue(&mut self, line: u64, error: LineError) -> bool {
        self.issues.push(Issue { line, error });
        self.issues.len() >= MAX_ISSUES
    }

    /// The whole-file checks after the last line, then the result.
    fn finish(
        mut self,
        endpoint: BatchEndpoint,
        force_gateway: bool,
    ) -> Result<Scanned, PlanError> {
        if self.requests == 0 && self.issues.is_empty() {
            self.issues.push(Issue {
                line: 1,
                error: LineError::EmptyFile,
            });
        }
        let native = !force_gateway
            && self.native_ok
            && self.deterministic_rejects.is_empty()
            && self.groups.len() == 1
            && self.groups[0]
                .adapter
                .native_batch(&self.groups[0].group.deployment, endpoint);
        if self.issues.is_empty() && !native {
            self.gateway_issue();
        }
        if self.issues.is_empty() && self.groups.is_empty() {
            self.report_rejects();
        }
        if !self.issues.is_empty() {
            return Err(PlanError::Invalid(self.issues));
        }
        let model = if self.groups.len() == 1 {
            self.groups[0].group.model.clone()
        } else {
            "mixed".to_owned()
        };
        let native = native.then(|| {
            (
                self.groups[0].group.deployment.clone(),
                self.groups[0].adapter.clone(),
            )
        });
        Ok((
            self.groups.into_iter().map(|g| g.group).collect(),
            self.requests,
            native,
            model,
        ))
    }

    /// A batch that runs through the gateway cannot hold lines the engine
    /// cannot run.
    fn gateway_issue(&mut self) {
        // Lines only a native batch could run.
        if let Some(g) = self.groups.iter().find(|g| g.legacy) {
            self.issues.push(Issue {
                line: g.first_line,
                error: LineError::ModelUnsupported,
            });
        } else if let Some(line) = self.gateway_unsupported {
            self.issues.push(Issue {
                line,
                error: LineError::UnsupportedFeature,
            });
        }
    }

    fn report_rejects(&mut self) {
        // Every candidate line was a deterministic adapter rejection:
        // there is nothing to reserve or dispatch. Refuse the upload
        // with a line-numbered report, like any other invalid line,
        // instead of admitting an empty batch.
        for (line, error) in std::mem::take(&mut self.deterministic_rejects) {
            let error = match error {
                InferenceError::InvalidRequest | InferenceError::InvalidField(_) => {
                    LineError::InvalidBody
                }
                InferenceError::Unsupported => LineError::UnsupportedFeature,
                other => {
                    tracing::warn!(error = ?other, "adapter request validation returned an error outside its contract");
                    LineError::UnsupportedFeature
                }
            };
            if self.issue(line, error) {
                break;
            }
        }
        if self.issues.is_empty() {
            self.issues.push(Issue {
                line: 1,
                error: LineError::EmptyFile,
            });
        }
    }
}
