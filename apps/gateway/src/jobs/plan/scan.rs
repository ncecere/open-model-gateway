//! Per-line batch checks: parse, model, price, engine support and the
//! adapter's deterministic refusal; then sizing and copying an accepted line.
use super::*;

/// A line that passed every per-line check.
pub(super) struct CheckedLine {
    model: String,
    resolved: Arc<Resolved>,
    /// The line's own output maximum (`Some(0)` for embeddings).
    requested: Option<u32>,
    price_id: Uuid,
    max: u32,
    deterministic_reject: Option<InferenceError>,
}

impl Jobs {
    /// Every per-line check, in order: shape, unique `custom_id`, model,
    /// price and bound, native/engine support and the adapter's own
    /// deterministic refusal.
    pub(super) async fn check_line(
        &self,
        scope: ScanScope<'_>,
        state: &mut ScanState,
        raw: &[u8],
    ) -> Result<Result<CheckedLine, LineError>, PlanError> {
        let line = match parse_line(raw, scope.endpoint) {
            Ok(l) => l,
            Err(e) => return Ok(Err(e)),
        };
        if !state.ids.insert(line.custom_id.clone()) {
            return Ok(Err(LineError::DuplicateCustomId));
        }
        let model = line.request.model().to_owned();
        let resolved = match self.resolve_once(scope, &mut state.models, &model).await? {
            Ok(r) => r,
            Err(e) => return Ok(Err(e)),
        };
        let requested = match &line.request {
            BatchRequest::Chat(r) => r.max_output_tokens,
            BatchRequest::Embeddings(_) => Some(0),
        };
        let (price_id, max) = match self
            .price_line(scope, state, &line.request, &resolved, requested)
            .await?
        {
            Ok(priced) => priced,
            Err(e) => return Ok(Err(e)),
        };
        state.prices.insert(resolved.deployment.id, price_id);
        // Encoding checks use the same bound as the parent hold; the
        // uploaded body and private body retain the optional omission.
        let mut execution_request = line.request.clone();
        if let BatchRequest::Chat(r) = &mut execution_request {
            r.max_output_tokens = Some(max);
        }
        let coarse_ok =
            match state.engine_checks(scope.endpoint, &line.request, &execution_request, &resolved)
            {
                Ok(coarse_ok) => coarse_ok,
                Err(e) => return Ok(Err(e)),
            };
        let mut checked = CheckedLine {
            model,
            resolved,
            requested,
            price_id,
            max,
            deterministic_reject: None,
        };
        checked.deterministic_reject = self
            .deterministic_reject(
                scope,
                &mut state.refusal_memo,
                &checked,
                &execution_request,
                coarse_ok,
            )
            .await?;
        Ok(Ok(checked))
    }

    /// The model's deployment for this key, resolved at most once per scan.
    async fn resolve_once(
        &self,
        scope: ScanScope<'_>,
        models: &mut HashMap<String, Result<Arc<Resolved>, LineError>>,
        model: &str,
    ) -> Result<Result<Arc<Resolved>, LineError>, PlanError> {
        if let std::collections::hash_map::Entry::Vacant(slot) = models.entry(model.to_owned()) {
            slot.insert(
                self.resolve(scope.principal, model, scope.endpoint, scope.request_id)
                    .await?
                    .map(Arc::new),
            );
        }
        Ok(models[model].clone())
    }

    /// The line's price and output maximum, previewed at most once per
    /// deployment, requested maximum and cache TTL mask.
    async fn price_line(
        &self,
        scope: ScanScope<'_>,
        state: &mut ScanState,
        request: &BatchRequest,
        resolved: &Resolved,
        requested: Option<u32>,
    ) -> Result<Result<(Uuid, u32), LineError>, PlanError> {
        let cache_controls = match request {
            BatchRequest::Chat(r) if resolved.deployment.provider == "anthropic" => r
                .messages_options
                .as_ref()
                .map(|o| o.cache_controls.as_slice())
                .unwrap_or_default(),
            _ => &[],
        };
        let cache_mask = cache_controls
            .iter()
            .fold(0_u8, |mask, c| mask | if c["ttl"] == "1h" { 2 } else { 1 });
        let key = (resolved.deployment.id, requested, cache_mask);
        if let std::collections::hash_map::Entry::Vacant(slot) = state.previews.entry(key) {
            slot.insert(
                crate::governance::batch::preview_line(
                    &self.store,
                    resolved.deployment.id,
                    scope.endpoint.protocol(),
                    requested,
                    state.prices.get(&resolved.deployment.id).copied(),
                    resolved.adapter.generation_output_limit(),
                    cache_controls,
                )
                .await?,
            );
        }
        Ok(match state.previews[&key] {
            LinePreview::Ok { price_id, output } => Ok((price_id, output)),
            LinePreview::OutputLimit => Err(LineError::OutputLimit),
            LinePreview::Unpriced | LinePreview::Unbounded => Err(LineError::Unpriced),
        })
    }

    /// The adapter's deterministic refusal of this exact line, if any.
    async fn deterministic_reject(
        &self,
        scope: ScanScope<'_>,
        memo: &mut RefusalMemo,
        line: &CheckedLine,
        execution_request: &BatchRequest,
        coarse_ok: bool,
    ) -> Result<Option<InferenceError>, JobError> {
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
        let resolved = &line.resolved;
        if resolved.legacy {
            return Ok(None);
        }
        Ok(match execution_request {
            BatchRequest::Chat(r) => {
                if line.requested.is_none() {
                    coarse_ok
                        .then(|| resolved.adapter.validate_chat_request(r).err())
                        .flatten()
                } else {
                    self.every_candidate_route_refuses(
                        scope.principal,
                        &line.model,
                        scope.endpoint.protocol(),
                        scope.request_id,
                        r,
                        memo,
                    )
                    .await?
                }
            }
            BatchRequest::Embeddings(_) => None,
        })
    }
}

/// Copy an accepted line to the work file.
pub(super) async fn copy_line(
    work: &mut LazySink,
    raw: Vec<u8>,
    line: &CheckedLine,
) -> Result<(), PlanError> {
    let mut copy = if line.requested.is_none() {
        lines::planned_copy(
            raw,
            lines::LinePlan {
                deployment: line.resolved.deployment.id,
                price: line.price_id,
                output: line.max,
            },
        )?
    } else {
        raw
    };
    copy.push(b'\n');
    work.raw(copy).await?;
    Ok(())
}

impl ScanState {
    /// Whether the line can run natively and through the engine (the
    /// coarse check). A legacy line must be encodable.
    fn engine_checks(
        &mut self,
        endpoint: BatchEndpoint,
        request: &BatchRequest,
        execution_request: &BatchRequest,
        resolved: &Resolved,
    ) -> Result<bool, LineError> {
        // Can this line run natively, and can the engine run it?
        let encodable = resolved
            .adapter
            .native_batch(&resolved.deployment, endpoint)
            && resolved
                .adapter
                .encode_native_line(&resolved.deployment, endpoint, "l0", execution_request)
                .is_ok();
        if resolved.legacy && !encodable {
            return Err(LineError::UnsupportedFeature);
        }
        self.native_ok &= encodable;
        let coarse_ok = resolved.legacy
            || match request {
                BatchRequest::Chat(r) => resolved.adapter.supports_chat_request(r),
                BatchRequest::Embeddings(r) => resolved
                    .adapter
                    .supports_embedding_target(&resolved.deployment, r),
            };
        if !coarse_ok && self.gateway_unsupported.is_none() {
            self.gateway_unsupported = Some(self.physical);
        }
        Ok(coarse_ok)
    }

    /// Size an accepted line into its deployment's group, or set it aside
    /// as a deterministic rejection.
    pub(super) fn record(&mut self, endpoint: BatchEndpoint, line: &CheckedLine) {
        let resolved = &line.resolved;
        if let Some(error) = line.deterministic_reject {
            // The engine refuses this line before it ever admits or
            // reserves anything for it (zero cost, #228): it must not
            // size or reserve the parent envelope either. It is still
            // copied below, so it still runs and is refused per line, at
            // the same classification as an interactive request, when
            // the batch dispatches (`jobs::runner`).
            self.deterministic_rejects.push((self.physical, error));
        } else {
            match self
                .groups
                .iter_mut()
                .find(|g| g.group.deployment.id == resolved.deployment.id)
            {
                Some(g) => {
                    *g.group.outputs.entry(line.max).or_insert(0) += 1;
                    if line.requested.is_none() {
                        g.group.price_id = Some(line.price_id);
                    }
                }
                None => self.groups.push(PendingGroup {
                    group: LineGroup {
                        deployment: resolved.deployment.clone(),
                        model: line.model.clone(),
                        protocol: endpoint.protocol(),
                        outputs: BTreeMap::from([(line.max, 1)]),
                        price_id: line.requested.is_none().then_some(line.price_id),
                    },
                    first_line: self.physical,
                    adapter: resolved.adapter.clone(),
                    legacy: resolved.legacy,
                }),
            }
        }
    }
}
