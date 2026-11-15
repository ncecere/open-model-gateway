//! The steps of [`super::validate`]: each establishes one part of a stored
//! rule, in the order the checks have always run (the first failing check
//! decides the error).
use super::*;

/// The trimmed name and the kind; built-in event kinds never have a
/// configurable rule, and workspace rules cannot use installation kinds.
pub(super) fn name_and_kind(b: &RuleInput, workspace: bool) -> Result<(String, Kind), ApiError> {
    let name = b.name.trim().to_owned();
    if !valid_name(&name) {
        return Err(invalid());
    }
    let kind = Kind::parse(&b.kind).ok_or_else(invalid)?;
    // Built-in event kinds never have a configurable rule.
    if matches!(kind, Kind::KeyExpiring | Kind::AgedUnknownHold) {
        return Err(invalid());
    }
    if workspace && matches!(kind, Kind::Provider | Kind::Spend | Kind::AdmissionCeiling) {
        return Err(invalid());
    }
    Ok((name, kind))
}

/// A rule with only the name, kind, enabled flag and notification switches.
pub(super) fn blank(name: String, kind: Kind, b: &RuleInput) -> Valid {
    Valid {
        name,
        kind,
        enabled: b.enabled,
        budget_layers: None,
        thresholds: None,
        spike_factor_percent: None,
        min_spend_microusd: None,
        window_minutes: None,
        error_rate_percent: None,
        min_requests: None,
        consecutive_failures: None,
        provider_connection_id: None,
        spend_period: None,
        spend_amount_microusd: None,
        ceiling_requests_per_second: None,
        ceiling_lock_wait_ms: None,
        notify_workspace_admins: b.notify_workspace_admins,
        notify_platform_admins: b.notify_platform_admins,
        notify_emails: Vec::new(),
    }
}

/// Fields that do not belong to the kind must be absent.
pub(super) fn only_fields_of_kind(b: &RuleInput, kind: Kind) -> Result<(), ApiError> {
    let budget = b.budget_layers.is_some() || b.thresholds.is_some();
    let spike = b.spike_factor_percent.is_some() || b.min_spend_microusd.is_some();
    let window =
        b.window_minutes.is_some() || b.error_rate_percent.is_some() || b.min_requests.is_some();
    let provider = b.consecutive_failures.is_some() || b.provider_connection_id.is_some();
    let spend = b.spend_period.is_some() || b.spend_amount_microusd.is_some();
    let ceiling = b.ceiling_requests_per_second.is_some() || b.ceiling_lock_wait_ms.is_some();
    // There is no installation budget layer any more (0026): say so plainly.
    if b.budget_layers
        .as_ref()
        .is_some_and(|l| l.iter().any(|l| l == "installation"))
    {
        return Err(super::super::installation_limits_removed(
            StatusCode::BAD_REQUEST,
        ));
    }
    let allowed = (!spend || kind == Kind::Spend) && (!ceiling || kind == Kind::AdmissionCeiling);
    let allowed = allowed
        && match kind {
            // Window plus a rate and/or lock-wait threshold; the incident
            // names the workspace, so email goes to Platform Admins only.
            Kind::AdmissionCeiling => {
                !budget
                    && !spike
                    && !provider
                    && b.error_rate_percent.is_none()
                    && b.min_requests.is_none()
                    && b.notify_emails.is_empty()
            }
            Kind::Budget => !spike && !window && !provider,
            Kind::Spend => b.budget_layers.is_none() && !spike && !window && !provider,
            Kind::Spike => !budget && !window && !provider,
            Kind::ErrorRate => !budget && !spike && !provider,
            Kind::Provider => !budget && !spike,
            Kind::BatchFailed => !budget && !spike && !window && !provider,
            Kind::BatchStalled => {
                !budget
                    && !spike
                    && !provider
                    && b.error_rate_percent.is_none()
                    && b.min_requests.is_none()
            }
            // Rejected above already; kept exhaustive for the compiler.
            Kind::KeyExpiring | Kind::AgedUnknownHold => false,
        };
    if !allowed {
        return Err(invalid());
    }
    Ok(())
}

/// Budget rules: the layers (sorted, distinct, known) and the thresholds.
pub(super) fn budget_parameters(
    v: &mut Valid,
    budget_layers: Option<Vec<String>>,
    percents: Option<Vec<i32>>,
) -> Result<(), ApiError> {
    let mut layers = budget_layers.ok_or_else(invalid)?;
    layers.sort_by_key(|l| BUDGET_LAYERS.iter().position(|x| x == l));
    layers.dedup();
    if layers.is_empty() || layers.iter().any(|l| !BUDGET_LAYERS.contains(&l.as_str())) {
        return Err(invalid());
    }
    v.budget_layers = Some(layers);
    v.thresholds = Some(thresholds(percents)?);
    Ok(())
}

/// Installation spend: percent thresholds of a reference amount per period.
pub(super) fn spend_parameters(
    v: &mut Valid,
    spend_period: Option<String>,
    spend_amount_microusd: Option<&str>,
    percents: Option<Vec<i32>>,
) -> Result<(), ApiError> {
    let period = spend_period.ok_or_else(invalid)?;
    if crate::governance::BudgetPeriod::parse(&period).is_none() {
        return Err(invalid());
    }
    // Like budget amounts: any positive i64.
    v.spend_amount_microusd = Some(microusd(spend_amount_microusd.ok_or_else(invalid)?, 19)?);
    v.spend_period = Some(period);
    v.thresholds = Some(thresholds(percents)?);
    Ok(())
}

/// Spend spikes: the factor and the minimum-spend floor.
pub(super) fn spike_parameters(
    v: &mut Valid,
    spike_factor_percent: Option<i32>,
    min_spend_microusd: Option<String>,
) -> Result<(), ApiError> {
    let factor = spike_factor_percent.ok_or_else(invalid)?;
    let floor = microusd(&min_spend_microusd.ok_or_else(invalid)?, 15)?;
    if !(110..=100_000).contains(&factor) {
        return Err(invalid());
    }
    v.spike_factor_percent = Some(factor);
    v.min_spend_microusd = Some(floor);
    Ok(())
}

/// Admission ceiling: a window plus a rate and/or lock-wait threshold.
pub(super) fn ceiling_parameters(
    v: &mut Valid,
    window_minutes: Option<i32>,
    ceiling_requests_per_second: Option<i32>,
    ceiling_lock_wait_ms: Option<i32>,
) -> Result<(), ApiError> {
    // Minute counters are retained 10 minutes (0024).
    let minutes = window_minutes.ok_or_else(invalid)?;
    if !(5..=10).contains(&minutes) {
        return Err(invalid());
    }
    v.window_minutes = Some(minutes);
    if let Some(rate) = ceiling_requests_per_second {
        if !(1..=100_000).contains(&rate) {
            return Err(invalid());
        }
        v.ceiling_requests_per_second = Some(rate);
    }
    if let Some(ms) = ceiling_lock_wait_ms {
        if !crate::governance::pressure::WAIT_BUCKETS_MS.contains(&ms) {
            return Err(invalid());
        }
        v.ceiling_lock_wait_ms = Some(ms);
    }
    if v.ceiling_requests_per_second.is_none() && v.ceiling_lock_wait_ms.is_none() {
        return Err(invalid());
    }
    Ok(())
}

/// Stalled batches: no progress for `window_minutes`.
pub(super) fn stalled_parameters(
    v: &mut Valid,
    window_minutes: Option<i32>,
) -> Result<(), ApiError> {
    let minutes = window_minutes.ok_or_else(invalid)?;
    if !(5..=1440).contains(&minutes) {
        return Err(invalid());
    }
    v.window_minutes = Some(minutes);
    Ok(())
}

/// Error-rate and provider rules: the window, the rate with its minimum
/// request count, and (provider rules) consecutive failures and connection.
pub(super) fn error_rate_parameters(v: &mut Valid, b: &RuleInput) -> Result<(), ApiError> {
    let kind = v.kind;
    let minutes = b.window_minutes.ok_or_else(invalid)?;
    if !(5..=1440).contains(&minutes) {
        return Err(invalid());
    }
    v.window_minutes = Some(minutes);
    match (b.error_rate_percent, b.min_requests) {
        (Some(rate), Some(min)) if (1..=100).contains(&rate) && (1..=100_000).contains(&min) => {
            v.error_rate_percent = Some(rate);
            v.min_requests = Some(min);
        }
        (None, None) if kind == Kind::Provider => {}
        _ => return Err(invalid()),
    }
    if kind == Kind::Provider {
        if let Some(n) = b.consecutive_failures {
            if !(1..=100).contains(&n) {
                return Err(invalid());
            }
            v.consecutive_failures = Some(n);
        }
        if v.consecutive_failures.is_none() && v.error_rate_percent.is_none() {
            return Err(invalid());
        }
        v.provider_connection_id = b.provider_connection_id;
    }
    Ok(())
}

/// Installation rules email Platform Admins or listed addresses; workspace
/// admins belong to workspace rules. The addresses are trimmed, lowercased
/// and de-duplicated.
pub(super) fn notification_emails(
    v: &mut Valid,
    notify_emails: Vec<String>,
    workspace: bool,
) -> Result<(), ApiError> {
    if !workspace && v.notify_workspace_admins {
        return Err(invalid());
    }
    if notify_emails.len() > EMAILS {
        return Err(invalid());
    }
    let mut emails = Vec::new();
    for e in notify_emails {
        let e = e.trim().to_lowercase();
        if !directory::email_valid(&e) || e.parse::<lettre::Address>().is_err() {
            return Err(invalid());
        }
        if !emails.contains(&e) {
            emails.push(e);
        }
    }
    v.notify_emails = emails;
    Ok(())
}
