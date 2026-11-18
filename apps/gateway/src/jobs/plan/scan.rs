//! Per-line route selection, price preview and sizing for batch admission.
use super::*;

impl Jobs {
    /// Size an executable line and return the pin required for its private copy.
    pub(super) async fn check_line(
        &self,
        scope: ScanScope<'_>,
        state: &mut ScanState,
        raw: &[u8],
    ) -> Result<Result<Option<lines::LinePlan>, LineError>, PlanError> {
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
        let resolved = if let BatchRequest::Chat(request) = &line.request
            && requested.is_some()
            && !matches!(&resolved, PreferredRoute::Ready(route) if route.legacy)
        {
            match self
                .explicit_chat_route(scope, resolved, request, &mut state.route_memo)
                .await
            {
                Ok(Ok(route)) => route,
                Err(JobError::Inference(InferenceError::ModelUnavailable)) => {
                    return Ok(Err(LineError::ModelNotFound));
                }
                Err(error) => return Err(error.into()),
                Ok(Err(error)) => {
                    state.deterministic_rejects.push((state.physical, error));
                    return Ok(Ok(None));
                }
            }
        } else {
            resolved.live()?
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
        let pin = requested.is_none().then_some(lines::LinePlan {
            deployment: resolved.deployment.id,
            price: price_id,
            output: max,
        });
        if pin.is_some()
            && coarse_ok
            && !resolved.legacy
            && let BatchRequest::Chat(r) = &execution_request
            && let Err(error) = resolved.adapter.validate_chat_request(r).and_then(|()| {
                resolved
                    .adapter
                    .validate_tool_result_target(&resolved.deployment, r.has_failed_tool_result())
            })
        {
            state.deterministic_rejects.push((state.physical, error));
        } else if state.issues.is_empty() {
            state.record(scope.endpoint, &resolved, model, max, pin.map(|p| p.price));
        }
        Ok(Ok(pin))
    }

    /// The model's deployment for this key, resolved at most once per scan.
    async fn resolve_once(
        &self,
        scope: ScanScope<'_>,
        models: &mut HashMap<String, Result<PreferredRoute, LineError>>,
        model: &str,
    ) -> Result<Result<PreferredRoute, LineError>, PlanError> {
        if let std::collections::hash_map::Entry::Vacant(slot) = models.entry(model.to_owned()) {
            slot.insert(
                self.resolve(scope.principal, model, scope.endpoint, scope.request_id)
                    .await?,
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
}

/// Only omitted maxima carry a price and deployment pin into dispatch.
pub(super) async fn copy_line(
    work: &mut LazySink,
    raw: Vec<u8>,
    pin: Option<lines::LinePlan>,
) -> Result<(), PlanError> {
    let mut copy = if let Some(plan) = pin {
        lines::planned_copy(raw, plan)?
    } else {
        raw
    };
    copy.push(b'\n');
    work.raw(copy).await?;
    Ok(())
}

impl ScanState {
    /// Whether the engine can run the line (the coarse check). Native
    /// encodability is folded into `native_ok`; a legacy line must be
    /// encodable.
    fn engine_checks(
        &mut self,
        endpoint: BatchEndpoint,
        request: &BatchRequest,
        execution_request: &BatchRequest,
        resolved: &Resolved,
    ) -> Result<bool, LineError> {
        // Can this line run natively, and can the engine run it?
        let encodable = !resolved.pricing_only
            && resolved
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

    /// Size a runnable line into its selected deployment's parent envelope.
    fn record(
        &mut self,
        endpoint: BatchEndpoint,
        resolved: &Resolved,
        model: String,
        max: u32,
        pinned_price: Option<Uuid>,
    ) {
        match self
            .groups
            .iter_mut()
            .find(|g| g.group.deployment.id == resolved.deployment.id)
        {
            Some(g) => {
                *g.group.outputs.entry(max).or_insert(0) += 1;
                if pinned_price.is_some() {
                    g.group.price_id = pinned_price;
                }
            }
            None => self.groups.push(PendingGroup {
                group: LineGroup {
                    deployment: resolved.deployment.clone(),
                    model,
                    protocol: endpoint.protocol(),
                    outputs: BTreeMap::from([(max, 1)]),
                    price_id: pinned_price,
                },
                first_line: self.physical,
                adapter: resolved.adapter.clone(),
                legacy: resolved.legacy,
            }),
        }
    }
}
