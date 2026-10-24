//! Frozen aggregate comparison against bounded raw history, never hourly
//! rollups or today's public aliases. No per-request allocation or prorating.
use super::*;
use chrono::Duration;
use std::collections::{BTreeMap, BTreeSet};

type ActualKey = (
    DateTime<Utc>,
    Option<String>,
    Option<String>,
    Option<String>,
    String,
);
type EstimateKey = (DateTime<Utc>, Uuid, Option<String>, Option<String>);
type IndexKey = (DateTime<Utc>, Uuid, Option<String>, Option<String>);
type ScopeKey = (DateTime<Utc>, Uuid, Option<Uuid>);
type ClaimKey = (DateTime<Utc>, Uuid, Option<Uuid>, Option<String>, String);
type ObservationIndex<'a> = BTreeMap<IndexKey, Vec<(usize, &'a Observation)>>;
#[derive(Deserialize)]
struct Observation {
    day: DateTime<Utc>,
    started_at: DateTime<Utc>,
    workspace_id: Uuid,
    connection_id: Uuid,
    deployment_id: Uuid,
    upstream_model: Option<String>,
    reported_upstream_model: Option<String>,
    tier: Option<String>,
    reservation_state: Option<String>,
    settled: Option<String>,
    held: Option<String>,
    unbounded: bool,
    redacted: bool,
}
#[derive(Default)]
struct Estimate {
    settled: i128,
    held: i128,
    attempts: u64,
    unresolved: u64,
    unbounded: u64,
}
fn number(s: &str) -> Result<i128, ApiError> {
    s.parse().map_err(|_| invalid())
}
fn add(a: &mut i128, b: i128) -> Result<(), ApiError> {
    *a = a.checked_add(b).ok_or_else(invalid)?;
    Ok(())
}
impl Estimate {
    fn observe(&mut self, o: &Observation) -> Result<(), ApiError> {
        self.attempts += 1;
        if o.reservation_state.as_deref() == Some("settled") && o.settled.is_some() && !o.redacted {
            add(
                &mut self.settled,
                number(o.settled.as_deref().ok_or_else(invalid)?)?,
            )?;
        } else {
            // Preserve known settled subtotal even where model detail is gone.
            if let Some(v) = &o.settled {
                add(&mut self.settled, number(v)?)?;
            }
            self.unresolved += 1;
        }
        if o.reservation_state.as_deref() != Some("settled") {
            if let Some(v) = &o.held {
                add(&mut self.held, number(v)?)?;
            }
            if o.unbounded || o.held.is_none() {
                self.unbounded += 1;
            }
        }
        Ok(())
    }
    fn value(&self, archived: bool) -> Value {
        json!({"settled_estimate_microusd":self.settled.to_string(),"held_floor_microusd":self.held.to_string(),
            "holds_upper_bound_microusd":null,
            "attempts":self.attempts.to_string(),"unresolved_attempts":self.unresolved.to_string(),
            "unbounded_holds":self.unbounded.to_string(),"complete":self.unresolved==0 && !archived})
    }
}
#[derive(Default)]
struct Actual {
    amount: i128,
    dimensions: Vec<Value>,
}
fn scoped(binding: &Binding, o: &Observation) -> bool {
    o.connection_id == binding.connection_id
        && binding.deployment_id.is_none_or(|d| d == o.deployment_id)
        && o.started_at >= binding.effective_start
        && o.started_at < binding.effective_end
}
fn model_matches(binding: &Binding, model: &str, o: &Observation) -> bool {
    let explicit = binding
        .model_mappings
        .iter()
        .find(|m| m.provider_model == model);
    let expected = explicit.map_or(model, |m| m.upstream_model.as_str());
    let historical = o
        .reported_upstream_model
        .as_deref()
        .or(o.upstream_model.as_deref());
    historical == Some(expected)
        && (explicit.is_some()
            || o.reported_upstream_model.is_none()
            || o.reported_upstream_model == o.upstream_model)
}

pub(super) async fn snapshot(
    tx: &mut Transaction<'_, Postgres>,
    import_id: Uuid,
    mapping_id: Uuid,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    bindings: &[Binding],
) -> Result<Value, ApiError> {
    // The evidence, observation timestamp, archive status and raw rows come
    // from ONE statement snapshot, so concurrent settlement/archive cannot
    // tear the comparison. Only attested historical account scopes qualify.
    let raw:Value=sqlx::query_scalar("SELECT jsonb_build_object('observed_at',statement_timestamp(),'archived',EXISTS(SELECT FROM archived_partitions WHERE archive_group='history' AND lower_bound<$3 AND upper_bound>$2),'rows',(SELECT coalesce(jsonb_agg(to_jsonb(q)),'[]') FROM (SELECT date_trunc('day',e.started_at,'UTC') AS day,e.started_at,e.workspace_id,d.provider_connection_id AS connection_id,e.deployment_id,e.upstream_model,e.reported_upstream_model,r.price_tier AS tier,r.state AS reservation_state,r.actual_microusd::text AS settled,r.held_microusd::text AS held,coalesce(r.unbounded_cost,true) AS unbounded,e.details_redacted_at IS NOT NULL AS redacted FROM inference_executions e JOIN deployments d ON d.id=e.deployment_id LEFT JOIN governance_reservations r ON r.execution_id=e.id AND r.admitted_at=e.started_at WHERE e.provider='anthropic' AND e.started_at>=$2 AND e.started_at<$3 AND EXISTS(SELECT FROM provider_cost_bindings b WHERE b.mapping_id=$1 AND b.connection_id=d.provider_connection_id AND (b.deployment_id IS NULL OR b.deployment_id=e.deployment_id) AND e.started_at>=b.effective_start AND e.started_at<b.effective_end) ORDER BY e.started_at,e.id LIMIT 100001) q))")
        .bind(mapping_id).bind(start).bind(end).fetch_one(&mut **tx).await?;
    let observations: Vec<Observation> =
        serde_json::from_value(raw["rows"].clone()).map_err(|_| invalid())?;
    if observations.len() > 100_000 {
        return Err(ApiError(
            StatusCode::UNPROCESSABLE_ENTITY,
            "Raw history exceeds the 100000-attempt comparison bound; select a shorter import period",
        ));
    }
    let archived = raw["archived"].as_bool().unwrap_or(true);
    let lines:Vec<(DateTime<Utc>,Value,String)>=sqlx::query_as("SELECT bucket_start,dimensions,amount_microusd::text FROM provider_cost_lines WHERE import_id=$1 ORDER BY line_number")
        .bind(import_id).fetch_all(&mut **tx).await?;
    let mut actuals: BTreeMap<ActualKey, Actual> = BTreeMap::new();
    let mut supplied_categories = BTreeSet::new();
    for (day, dimensions, amount) in lines {
        let text = |key: &str| dimensions[key].as_str().map(str::to_owned);
        let category = text("cost_type").unwrap_or_else(|| "unknown".into());
        supplied_categories.insert(category.clone());
        let key = (
            day,
            text("workspace_id"),
            text("model"),
            text("service_tier"),
            category,
        );
        let a = actuals.entry(key).or_default();
        add(&mut a.amount, number(&amount)?)?;
        // Retain every supplied dimension, including unknown signed adjustments;
        // no guesses from free-text descriptions and no merged fee categories.
        a.dimensions.push(dimensions);
    }
    // Two provider model names mapping to the same historical aggregate do
    // not make that aggregate allocatable. Leave BOTH unmatched, rather than
    // whichever row happened to sort second (or double-counting estimates).
    let mut claims: BTreeMap<ClaimKey, usize> = BTreeMap::new();
    for (day, provider_workspace, model, tier, category) in actuals.keys() {
        if category != "tokens" {
            continue;
        }
        let Some(model) = model else {
            continue;
        };
        for b in bindings.iter().filter(|b| {
            b.provider_workspace_id == *provider_workspace
                && b.effective_start <= *day
                && b.effective_end >= *day + Duration::days(1)
        }) {
            let expected = b
                .model_mappings
                .iter()
                .find(|m| m.provider_model == *model)
                .map_or(model.as_str(), |m| m.upstream_model.as_str());
            *claims
                .entry((
                    *day,
                    b.connection_id,
                    b.deployment_id,
                    tier.clone(),
                    expected.to_owned(),
                ))
                .or_default() += 1;
        }
    }
    let mut rows = Vec::new();
    let mut used = BTreeSet::new();
    let mut total_actual = 0_i128;
    let mut total_estimate = Estimate::default();
    let mut index: ObservationIndex<'_> = BTreeMap::new();
    let mut workspaces: BTreeMap<ScopeKey, BTreeSet<Uuid>> = BTreeMap::new();
    for (position, o) in observations.iter().enumerate() {
        total_estimate.observe(o)?;
        let historical = o
            .reported_upstream_model
            .clone()
            .or(o.upstream_model.clone());
        index
            .entry((o.day, o.connection_id, historical, o.tier.clone()))
            .or_default()
            .push((position, o));
        for deployment in [None, Some(o.deployment_id)] {
            workspaces
                .entry((o.day, o.connection_id, deployment))
                .or_default()
                .insert(o.workspace_id);
        }
    }
    for ((day, provider_workspace, model, tier, category), actual) in actuals {
        add(&mut total_actual, actual.amount)?;
        let candidates: Vec<_> = bindings
            .iter()
            .filter(|b| {
                b.provider_workspace_id == provider_workspace
                    && b.effective_start <= day
                    && b.effective_end >= day + Duration::days(1)
            })
            .collect();
        let mut reasons = BTreeSet::from(["timing_basis"]);
        if archived {
            reasons.insert("incomplete_estimate");
        }
        let mut estimates: BTreeMap<Uuid, Estimate> = BTreeMap::new();
        let mut actual_workspace = None;
        if candidates.len() != 1 {
            let partial = bindings.iter().any(|b| {
                b.provider_workspace_id == provider_workspace
                    && b.effective_start < day + Duration::days(1)
                    && b.effective_end > day
            });
            reasons.insert(if partial {
                "ambiguous_scope"
            } else {
                "provider_only"
            });
        } else {
            let binding = candidates[0];
            let supported = category == "tokens"
                && matches!(tier.as_deref(), Some("standard" | "batch"))
                && actual.dimensions.iter().all(|d| {
                    matches!(
                        d["token_type"].as_str(),
                        Some(
                            "uncached_input_tokens"
                                | "output_tokens"
                                | "cache_read_input_tokens"
                                | "cache_creation.ephemeral_5m_input_tokens"
                                | "cache_creation.ephemeral_1h_input_tokens"
                        )
                    ) && matches!(d["context_window"].as_str(), Some("0-200k" | "200k-1M"))
                        && matches!(d["speed"].as_str(), None | Some("standard"))
                        && matches!(
                            d["inference_geo"].as_str(),
                            None | Some("global" | "not_available")
                        )
                });
            if !supported {
                reasons.insert("unsupported_category");
            } else if let Some(model) = &model {
                let expected = binding
                    .model_mappings
                    .iter()
                    .find(|m| m.provider_model == *model)
                    .map_or(model.as_str(), |m| m.upstream_model.as_str());
                let key = (
                    day,
                    binding.connection_id,
                    Some(expected.to_owned()),
                    tier.clone(),
                );
                let claim = (
                    day,
                    binding.connection_id,
                    binding.deployment_id,
                    tier.clone(),
                    expected.to_owned(),
                );
                let ambiguous = claims.get(&claim).is_some_and(|n| *n > 1);
                if ambiguous {
                    reasons.insert("ambiguous_scope");
                }
                for &(position, o) in index.get(&key).filter(|_| !ambiguous).into_iter().flatten() {
                    if scoped(binding, o) && model_matches(binding, model, o) {
                        if !used.insert(position) {
                            return Err(ApiError(
                                StatusCode::CONFLICT,
                                "Ambiguous aggregate model mapping would count estimates twice",
                            ));
                        }
                        estimates.entry(o.workspace_id).or_default().observe(o)?;
                    }
                }
                if estimates.is_empty() {
                    reasons.insert("unknown_model");
                    reasons.insert("provider_only");
                }
            } else {
                reasons.insert("unknown_model");
            }
            let scoped_workspaces =
                workspaces.get(&(day, binding.connection_id, binding.deployment_id));
            let conflicting_workspace = scoped_workspaces.is_some_and(|set| {
                set.len() > 1 || binding.workspace_id.is_none_or(|ws| !set.contains(&ws))
            });
            if binding.exclusive && !binding.non_omg_traffic && !conflicting_workspace {
                actual_workspace = binding.workspace_id;
            } else {
                reasons.insert("shared_unallocated");
                if binding.exclusive {
                    reasons.insert("ambiguous_scope");
                }
            }
        }
        if actual_workspace.is_none() {
            reasons.insert("shared_unallocated");
        }
        if estimates.values().any(|e| e.unresolved > 0) {
            reasons.insert("incomplete_estimate");
        }
        let mut estimated = 0_i128;
        for e in estimates.values() {
            add(&mut estimated, e.settled)?;
        }
        let variance = if actual_workspace.is_some()
            && !estimates.is_empty()
            && !reasons.contains("incomplete_estimate")
            && !reasons.contains("unsupported_category")
        {
            Some(
                actual
                    .amount
                    .checked_sub(estimated)
                    .ok_or_else(invalid)?
                    .to_string(),
            )
        } else {
            None
        };
        let workspace_estimates: Vec<Value> = estimates
            .into_iter()
            .map(|(ws, e)| json!({"workspace_id":ws,"totals":e.value(archived)}))
            .collect();
        rows.push(json!({"day":day,"provider_workspace_id":provider_workspace,"model":model,"service_tier":tier,"category":category,
            "dimensions":actual.dimensions,"workspace_id":actual_workspace,"imported_microusd":actual.amount.to_string(),
            "variance_microusd":variance,"workspace_estimates":workspace_estimates,"reasons":reasons}));
    }
    let mut estimate_only: BTreeMap<EstimateKey, Estimate> = BTreeMap::new();
    for (index, o) in observations.iter().enumerate() {
        if !used.contains(&index) {
            let key = (
                o.day,
                o.workspace_id,
                o.reported_upstream_model
                    .clone()
                    .or(o.upstream_model.clone()),
                o.tier.clone(),
            );
            estimate_only.entry(key).or_default().observe(o)?;
        }
    }
    for ((day, workspace, model, tier), estimate) in estimate_only {
        let mut reasons = vec!["estimate_only", "timing_basis"];
        if archived || estimate.unresolved > 0 {
            reasons.push("incomplete_estimate");
        }
        rows.push(json!({"day":day,"workspace_id":workspace,"model":model,"service_tier":tier,
            "imported_microusd":null,"variance_microusd":null,"estimate":estimate.value(archived),"reasons":reasons}));
    }
    let category_coverage: Vec<Value> = [
        ("usage", &["tokens"][..]), ("discounts", &["discount", "discounts"][..]),
        ("credits", &["credit", "credits"][..]), ("taxes", &["tax", "taxes"][..]),
        ("tools", &["web_search", "code_execution", "session_usage"][..]),
    ].into_iter().map(|(category,names)| json!({"category":category,"status":if names.iter().any(|n|supplied_categories.contains(*n)) {"supplied"} else {"not_supplied"}})).collect();
    Ok(
        json!({"name":"provider-reported cost reconciliation","verified_invoice":false,"complete":false,
        "import_id":import_id,"mapping_id":mapping_id,"query_version":"aggregate-attempt-start-v1",
        "observed_at":raw["observed_at"],"period":{"start":start,"end":end},
        "timing_basis":{"omg":"attempt_start_utc","provider":"provider_accounting_bucket","equivalent":false},
        "not_supplied":["invoice_completeness","priority_tier"],"category_coverage":category_coverage,
        "archived_detail_missing":archived,"unmapped_account_scopes":"not_inferred",
        "mapping_provenance":bindings,"imported_microusd":total_actual.to_string(),
        "estimates":total_estimate.value(archived),"rows":rows}),
    )
}
