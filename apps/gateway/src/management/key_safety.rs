//! Key safety audit: read-only findings for active API keys (docs/key-safety.md).
//!
//! Visibility follows the API keys list:
//! - workspace scope: shared administrators (and a personal owner) see every
//!   active key of the workspace, members their own human keys;
//! - platform scope (Admin/Auditor): Team/Project keys only, without key names
//!   or holder identities (admin views name them "API key in <workspace>").
//!   Personal keys contribute aggregate counts only, never rows or identifiers.
//!
//! Findings are computed from live configuration on every request; nothing is
//! stored and no key is changed. Effective limits compose every applicable
//! layer: installation, type default (or its platform replacement), workspace
//! local and key lineage.
use super::*;
use chrono::{DateTime, TimeDelta, Utc};

/// A workspace with at least this many usable models makes an unrestricted key "broad".
pub(crate) const BROAD_MODELS: i64 = 5;
/// A never-used key is reported after this grace (or `unused_days`, if smaller).
pub(crate) const NEVER_USED_GRACE_DAYS: i64 = 7;
/// Platform rows returned at most (counts always cover every key).
const MAX_ROWS: usize = 500;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Severity {
    Low,
    Medium,
    High,
}
impl Severity {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Finding {
    pub(crate) code: &'static str,
    pub(crate) severity: Severity,
    /// `days` for age/expiry findings, `models` for broad access.
    pub(crate) value: Option<(&'static str, i64)>,
}
impl Finding {
    fn json(&self) -> Value {
        let mut v = json!({"code":self.code,"severity":self.severity.as_str()});
        if let Some((k, n)) = self.value {
            v[k] = json!(n);
        }
        v
    }
}
#[derive(Clone, Copy, Debug)]
pub(crate) struct Thresholds {
    pub(crate) unused_days: i64,
    pub(crate) rotation_days: i64,
    pub(crate) max_lifetime_days: i64,
}
/// Live facts about one active key, as loaded by [`FACTS`].
#[derive(Clone, Debug, sqlx::FromRow)]
pub(crate) struct Facts {
    pub(crate) id: Uuid,
    pub(crate) name: String,
    pub(crate) workspace_id: Uuid,
    pub(crate) workspace_name: String,
    pub(crate) workspace_kind: String,
    pub(crate) issued_to_user_id: Option<Uuid>,
    pub(crate) service_account_id: Option<Uuid>,
    pub(crate) created_at: DateTime<Utc>,
    pub(crate) expires_at: Option<DateTime<Utc>>,
    pub(crate) last_used_at: Option<DateTime<Utc>>,
    pub(crate) restricted: bool,
    pub(crate) workspace_models: i64,
    pub(crate) has_budget: bool,
    pub(crate) has_caps: bool,
    pub(crate) holder_has_access: bool,
}
fn days(d: TimeDelta) -> i64 {
    d.num_days()
}
/// Every finding for one key, most severe first. Pure: `now` and thresholds are inputs.
pub(crate) fn findings(f: &Facts, now: DateTime<Utc>, t: &Thresholds) -> Vec<Finding> {
    let mut out = Vec::new();
    let mut push = |code, severity, value| {
        out.push(Finding {
            code,
            severity,
            value,
        })
    };
    let human = f.issued_to_user_id.is_some();
    match f.expires_at {
        None => push("no_expiry", Severity::High, None),
        // Human keys follow the installation maximum (Admin › Settings › General).
        Some(exp) if human && exp > now + TimeDelta::days(t.max_lifetime_days) => push(
            "expiry_beyond_max",
            Severity::Medium,
            Some(("days", days(exp - now))),
        ),
        Some(_) => {}
    }
    if !f.has_budget && !f.has_caps {
        push("no_limits", Severity::High, None);
    } else if !f.has_budget {
        push("no_budget", Severity::Medium, None);
    }
    // Defensive: every API path that ends a shared membership revokes the holder's
    // keys in the same transaction (manual removal, group/mapping loss at sign-in
    // or via SCIM, SCIM deactivation, suspension, role loss, workspace disable,
    // cleanup), so this finding should not appear in normal operation. It stays
    // because access is re-checked live and a key can still outlive its holder's
    // membership through changes made outside these paths (database restores,
    // direct SQL, a future reinstatement flow); a High finding is the safe signal.
    if human && f.workspace_kind != "personal" && !f.holder_has_access {
        push("owner_lost_access", Severity::High, None);
    }
    match f.last_used_at {
        Some(used) if days(now - used) >= t.unused_days => {
            push("unused", Severity::Low, Some(("days", days(now - used))))
        }
        None if days(now - f.created_at) >= t.unused_days.min(NEVER_USED_GRACE_DAYS) => push(
            "never_used",
            Severity::Low,
            Some(("days", days(now - f.created_at))),
        ),
        _ => {}
    }
    if !f.restricted && f.workspace_models >= BROAD_MODELS {
        push(
            "broad_model_access",
            Severity::Low,
            Some(("models", f.workspace_models)),
        );
    }
    // The current secret's age: rotation issues a new credential in the same lineage.
    if days(now - f.created_at) >= t.rotation_days {
        push(
            "not_rotated",
            Severity::Medium,
            Some(("days", days(now - f.created_at))),
        );
    }
    out.sort_by_key(|f| std::cmp::Reverse(f.severity));
    out
}
/// Active keys of live workspaces with their effective-limit facts. `{SCOPE}` restricts `k`/`w`.
const FACTS: &str = "WITH ks AS (SELECT k.id,k.name,k.workspace_id,w.name workspace_name,w.kind workspace_kind,k.issued_to_user_id,k.service_account_id,k.created_at,k.expires_at,k.governance_key_id,(SELECT max(e.started_at) FROM inference_executions e WHERE e.api_key_id=k.id) last_used_at,EXISTS(SELECT 1 FROM key_model_restrictions r WHERE r.workspace_id=k.workspace_id AND r.governance_key_id=k.governance_key_id) restricted,EXISTS(SELECT 1 FROM workspace_platform_policy_overrides o WHERE o.workspace_id=w.id) overridden FROM api_keys k JOIN workspaces w ON w.id=k.workspace_id WHERE w.disabled_at IS NULL AND k.revoked_at IS NULL AND k.disabled_at IS NULL AND (k.expires_at IS NULL OR k.expires_at>now()) AND {SCOPE}),
wm AS (SELECT g.workspace_id,count(DISTINCT g.model_id) n FROM workspace_model_grants g WHERE g.workspace_id IN(SELECT workspace_id FROM ks) AND workspace_model_allowed(g.workspace_id,g.model_id) GROUP BY g.workspace_id)
SELECT ks.id,ks.name,ks.workspace_id,ks.workspace_name,ks.workspace_kind,ks.issued_to_user_id,ks.service_account_id,ks.created_at,ks.expires_at,ks.last_used_at,ks.restricted,coalesce(wm.n,0) workspace_models,
EXISTS(SELECT 1 FROM policy_budgets b WHERE b.layer='installation' OR (b.layer='type' AND NOT ks.overridden AND b.kind=ks.workspace_kind) OR (b.layer='override' AND ks.overridden AND b.workspace_id=ks.workspace_id) OR (b.layer='local' AND b.workspace_id=ks.workspace_id) OR (b.layer='key' AND b.workspace_id=ks.workspace_id AND b.governance_key_id=ks.governance_key_id)) has_budget,
(EXISTS(SELECT 1 FROM installation_policy p WHERE {CAPPED}) OR (NOT ks.overridden AND EXISTS(SELECT 1 FROM workspace_type_policies p WHERE p.kind=ks.workspace_kind AND {CAPPED})) OR (ks.overridden AND EXISTS(SELECT 1 FROM workspace_platform_policy_overrides p WHERE p.workspace_id=ks.workspace_id AND {CAPPED})) OR EXISTS(SELECT 1 FROM workspace_local_policies p WHERE p.workspace_id=ks.workspace_id AND {CAPPED}) OR EXISTS(SELECT 1 FROM key_policies p WHERE p.workspace_id=ks.workspace_id AND p.governance_key_id=ks.governance_key_id AND {CAPPED})) has_caps,
(ks.issued_to_user_id IS NULL OR ks.workspace_kind='personal' OR EXISTS(SELECT 1 FROM effective_workspace_memberships m WHERE m.workspace_id=ks.workspace_id AND m.user_id=ks.issued_to_user_id)) holder_has_access
FROM ks LEFT JOIN wm ON wm.workspace_id=ks.workspace_id ORDER BY ks.workspace_name,ks.workspace_id,ks.created_at DESC,ks.id";
const CAPPED: &str =
    "coalesce(p.requests_per_minute,p.tokens_per_minute,p.concurrent_requests) IS NOT NULL";
/// `$1` workspace, `$2` all keys (else only `$3`'s human keys), `$4` optional key.
const WORKSPACE_SCOPE: &str =
    "k.workspace_id=$1 AND ($2 OR k.issued_to_user_id=$3) AND ($4::uuid IS NULL OR k.id=$4)";
/// Platform: every workspace kind is loaded, personal rows only ever become counts.
const PLATFORM_SCOPE: &str = "($1::uuid IS NULL OR k.workspace_id=$1) AND $2::boolean IS NOT NULL AND $3::uuid IS NOT NULL AND $4::uuid IS NULL";

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub(super) struct SafetyQuery {
    unused_days: Option<i64>,
    rotation_days: Option<i64>,
    /// Workspace scope only: one key (the key page).
    key_id: Option<Uuid>,
    /// Platform scope only: one Team/Project.
    workspace_id: Option<Uuid>,
}
impl SafetyQuery {
    fn thresholds(&self, max_lifetime_days: i64) -> Result<Thresholds, ApiError> {
        let unused_days = self.unused_days.unwrap_or(30);
        let rotation_days = self.rotation_days.unwrap_or(180);
        if !(1..=365).contains(&unused_days) || !(7..=1095).contains(&rotation_days) {
            return Err(invalid());
        }
        Ok(Thresholds {
            unused_days,
            rotation_days,
            max_lifetime_days,
        })
    }
}
struct Assessed {
    facts: Facts,
    findings: Vec<Finding>,
}
impl Assessed {
    fn severity(&self) -> Option<Severity> {
        self.findings.iter().map(|f| f.severity).max()
    }
}
async fn load(
    tx: &mut Transaction<'_, Postgres>,
    scope: &str,
    binds: (Option<Uuid>, bool, Uuid, Option<Uuid>),
    q: &SafetyQuery,
) -> Result<(Vec<Assessed>, Thresholds), ApiError> {
    sqlx::query("SET LOCAL statement_timeout='10s'")
        .execute(&mut **tx)
        .await?;
    let max: i32 = sqlx::query_scalar(
        "SELECT human_key_max_lifetime_days FROM installation_settings WHERE singleton",
    )
    .fetch_one(&mut **tx)
    .await?;
    let t = q.thresholds(i64::from(max))?;
    let now: DateTime<Utc> = sqlx::query_scalar("SELECT now()")
        .fetch_one(&mut **tx)
        .await?;
    let sql = FACTS.replace("{SCOPE}", scope).replace("{CAPPED}", CAPPED);
    let rows: Vec<Facts> = sqlx::query_as(&sql)
        .bind(binds.0)
        .bind(binds.1)
        .bind(binds.2)
        .bind(binds.3)
        .fetch_all(&mut **tx)
        .await?;
    let assessed = rows
        .into_iter()
        .map(|facts| Assessed {
            findings: findings(&facts, now, &t),
            facts,
        })
        .collect();
    Ok((assessed, t))
}
/// Keys counted by their most severe finding.
fn summary<'a>(rows: impl Iterator<Item = &'a Assessed>) -> Value {
    let (mut keys, mut flagged, mut high, mut medium, mut low) = (0u64, 0u64, 0u64, 0u64, 0u64);
    for r in rows {
        keys += 1;
        match r.severity() {
            Some(Severity::High) => high += 1,
            Some(Severity::Medium) => medium += 1,
            Some(Severity::Low) => low += 1,
            None => continue,
        }
        flagged += 1;
    }
    json!({"keys":keys,"flagged":flagged,"high":high,"medium":medium,"low":low})
}
fn thresholds_json(t: &Thresholds) -> Value {
    json!({"unused_days":t.unused_days,"rotation_days":t.rotation_days,"max_lifetime_days":t.max_lifetime_days,"broad_models":BROAD_MODELS,"never_used_grace_days":NEVER_USED_GRACE_DAYS})
}
/// Flagged rows, most severe first, then workspace and newest key.
fn flagged(rows: &[Assessed]) -> Vec<&Assessed> {
    let mut out: Vec<&Assessed> = rows.iter().filter(|r| r.severity().is_some()).collect();
    out.sort_by_key(|r| std::cmp::Reverse(r.severity()));
    out
}
fn findings_json(r: &Assessed) -> Value {
    json!(r.findings.iter().map(Finding::json).collect::<Vec<_>>())
}

/// GET /workspaces/{ws}/key-safety: findings for the keys the caller may list.
pub(super) async fn workspace_key_safety(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(ws): Path<Uuid>,
    Query(q): Query<SafetyQuery>,
) -> ApiResult {
    if q.workspace_id.is_some() {
        return Err(invalid());
    }
    let (mut tx, a) = resources::workspace_tx(&s, &u, ws).await?;
    resources::detail_access(&a)?;
    let (rows, t) = load(
        &mut tx,
        WORKSPACE_SCOPE,
        (Some(ws), a.admin, u.user_id, q.key_id),
        &q,
    )
    .await?;
    tx.commit().await?;
    let data: Vec<Value> = flagged(&rows)
        .into_iter()
        .map(|r| {
            let f = &r.facts;
            let holder = if f.service_account_id.is_some() {
                "service_account"
            } else if f.issued_to_user_id == Some(u.user_id) {
                "you"
            } else {
                "member"
            };
            json!({"key":{"id":f.id,"name":f.name,"workspace":{"id":f.workspace_id,"name":f.workspace_name,"kind":f.workspace_kind},"holder":holder,"issued_to_user_id":f.issued_to_user_id,"service_account_id":f.service_account_id,"created_at":f.created_at,"expires_at":f.expires_at,"last_used_at":f.last_used_at,"model_restricted":f.restricted},"severity":r.severity().map(Severity::as_str),"findings":findings_json(r)})
        })
        .collect();
    Ok(Json(
        json!({"scope":"workspace","summary":summary(rows.iter()),"data":data,"thresholds":thresholds_json(&t)}),
    ))
}

/// GET /platform/key-safety: Team/Project findings plus personal aggregate counts.
pub(super) async fn platform_key_safety(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Query(q): Query<SafetyQuery>,
) -> ApiResult {
    if q.key_id.is_some() {
        return Err(invalid());
    }
    let mut tx = resources::installation_tx(&s).await?;
    resources::platform_read(&mut tx, u.user_id).await?;
    if let Some(ws) = q.workspace_id {
        // Only a live Team/Project can be filtered on; personal workspaces are never itemized.
        sqlx::query_scalar::<_, Uuid>(
            "SELECT id FROM workspaces WHERE id=$1 AND kind IN('team','project') AND disabled_at IS NULL",
        )
        .bind(ws)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(missing)?;
    }
    let (rows, t) = load(
        &mut tx,
        PLATFORM_SCOPE,
        (q.workspace_id, true, u.user_id, None),
        &q,
    )
    .await?;
    tx.commit().await?;
    let (shared_rows, personal_rows): (Vec<Assessed>, Vec<Assessed>) = rows
        .into_iter()
        .partition(|r| shared(&r.facts.workspace_kind));
    let list = flagged(&shared_rows);
    let truncated = list.len() > MAX_ROWS;
    // No key name, holder identity or service account: "API key in <workspace>".
    let data: Vec<Value> = list
        .into_iter()
        .take(MAX_ROWS)
        .map(|r| {
            let f = &r.facts;
            json!({"key":{"id":f.id,"workspace":{"id":f.workspace_id,"name":f.workspace_name,"kind":f.workspace_kind},"holder":if f.service_account_id.is_some() {"service_account"} else {"member"},"created_at":f.created_at,"expires_at":f.expires_at,"last_used_at":f.last_used_at,"model_restricted":f.restricted},"severity":r.severity().map(Severity::as_str),"findings":findings_json(r)})
        })
        .collect();
    let personal = q
        .workspace_id
        .is_none()
        .then(|| summary(personal_rows.iter()));
    Ok(Json(
        json!({"scope":"platform","summary":summary(shared_rows.iter()),"personal":personal,"data":data,"truncated":truncated,"thresholds":thresholds_json(&t)}),
    ))
}
pub(super) fn routes() -> Router<Store> {
    Router::new()
        .route(
            "/api/v1/workspaces/{ws}/key-safety",
            get(workspace_key_safety),
        )
        .route("/api/v1/platform/key-safety", get(platform_key_safety))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn facts() -> Facts {
        let now = Utc::now();
        Facts {
            id: Uuid::nil(),
            name: "k".into(),
            workspace_id: Uuid::nil(),
            workspace_name: "Team".into(),
            workspace_kind: "team".into(),
            issued_to_user_id: Some(Uuid::nil()),
            service_account_id: None,
            created_at: now - TimeDelta::days(1),
            expires_at: Some(now + TimeDelta::days(30)),
            last_used_at: Some(now),
            restricted: true,
            workspace_models: 1,
            has_budget: true,
            has_caps: true,
            holder_has_access: true,
        }
    }
    const T: Thresholds = Thresholds {
        unused_days: 30,
        rotation_days: 180,
        max_lifetime_days: 90,
    };
    fn codes(f: &Facts) -> Vec<&'static str> {
        findings(f, Utc::now(), &T).iter().map(|f| f.code).collect()
    }
    #[test]
    fn healthy_key_has_no_findings() {
        assert!(codes(&facts()).is_empty());
    }
    #[test]
    fn expiry_rules_apply_lifetime_maximum_to_human_keys_only() {
        let mut f = facts();
        f.expires_at = None;
        assert_eq!(codes(&f), ["no_expiry"]);
        f.expires_at = Some(Utc::now() + TimeDelta::days(200));
        assert_eq!(codes(&f), ["expiry_beyond_max"]);
        f.issued_to_user_id = None;
        f.service_account_id = Some(Uuid::nil());
        assert!(codes(&f).is_empty());
    }
    #[test]
    fn limits_are_effective_across_layers() {
        let mut f = facts();
        f.has_budget = false;
        assert_eq!(codes(&f), ["no_budget"]);
        f.has_caps = false;
        let all = findings(&f, Utc::now(), &T);
        assert_eq!(all[0].code, "no_limits");
        assert_eq!(all[0].severity, Severity::High);
    }
    #[test]
    fn usage_rotation_and_broad_access() {
        let now = Utc::now();
        let mut f = facts();
        f.last_used_at = Some(now - TimeDelta::days(45));
        assert_eq!(codes(&f), ["unused"]);
        f.last_used_at = None;
        // New keys get a grace period before "never used".
        assert!(codes(&f).is_empty());
        f.created_at = now - TimeDelta::days(200);
        f.expires_at = Some(now + TimeDelta::days(10));
        let all = findings(&f, now, &T);
        assert_eq!(
            all.iter().map(|x| x.code).collect::<Vec<_>>(),
            ["not_rotated", "never_used"]
        );
        assert_eq!(all[0].value, Some(("days", 200)));
        let mut f = facts();
        f.restricted = false;
        f.workspace_models = BROAD_MODELS - 1;
        assert!(codes(&f).is_empty());
        f.workspace_models = BROAD_MODELS;
        assert_eq!(codes(&f), ["broad_model_access"]);
    }
    #[test]
    fn owner_lost_access_is_for_shared_human_keys_only() {
        let mut f = facts();
        f.holder_has_access = false;
        assert_eq!(codes(&f), ["owner_lost_access"]);
        f.workspace_kind = "personal".into();
        assert!(codes(&f).is_empty());
        f.workspace_kind = "project".into();
        f.issued_to_user_id = None;
        f.service_account_id = Some(Uuid::nil());
        assert!(codes(&f).is_empty());
    }
    #[test]
    fn thresholds_are_validated() {
        let q = |u, r| SafetyQuery {
            unused_days: u,
            rotation_days: r,
            ..Default::default()
        };
        assert!(q(None, None).thresholds(365).is_ok());
        assert!(q(Some(0), None).thresholds(365).is_err());
        assert!(q(None, Some(2000)).thresholds(365).is_err());
    }
}
