//! Effective access by layer and "why can't I use this model?" reasons.
//! Read-only; same visibility as the workspace/key policy GET. Installation
//! usage never contributes reasons (no headroom disclosure).
use super::*;
use crate::governance::{BudgetPeriod, budget_consumption};
use chrono::{DateTime, Utc};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct AccessQuery {
    model_id: Option<Uuid>,
}
const LAYERS: [&str; 5] = [
    "platform",
    "type_default",
    "workspace_override",
    "workspace",
    "key",
];
fn rank(layer: &str) -> usize {
    LAYERS.iter().position(|l| *l == layer).unwrap_or(0)
}
#[derive(Clone)]
struct Reason {
    code: &'static str,
    layer: &'static str,
    period: Option<BudgetPeriod>,
    blocking: bool,
}
impl Reason {
    fn json(&self) -> Value {
        let mut v = json!({"code":self.code,"layer":self.layer});
        if let Some(p) = self.period {
            v["period"] = json!(p.as_str());
        }
        v
    }
}
fn status(reasons: &[Reason], upto: usize) -> &'static str {
    let applicable = reasons.iter().filter(|r| rank(r.layer) <= upto);
    let mut partial = false;
    for r in applicable {
        if r.blocking {
            return "unavailable";
        }
        partial = true;
    }
    if partial { "partial" } else { "available" }
}
fn rates(l: &governance::Limits) -> Value {
    json!({"requests_per_minute":l.requests_per_minute,"tokens_per_minute":l.tokens_per_minute,"concurrent_requests":l.concurrent_requests,"concurrent_jobs":l.concurrent_jobs})
}
type ModelRow = (
    Uuid,
    String,
    String,
    bool,
    bool,
    bool,
    Option<bool>,
    i64,
    i64,
);
async fn access(
    s: Store,
    u: BrowserPrincipal,
    ws: Uuid,
    key: Option<Uuid>,
    model: Option<Uuid>,
) -> ApiResult {
    // Platform readers may inspect a disabled Team/Project read-only; keys stay member-only.
    let (mut tx, a) = match key {
        Some(_) => resources::workspace_tx(&s, &u, ws).await?,
        None => resources::workspace_read_tx(&s, &u, ws).await?,
    };
    let lineage = match key {
        Some(k) => Some(governance::lineage(&mut tx, &u, ws, k, a.admin || a.owner).await?),
        None => None,
    };
    let l = governance::layers_with(&mut tx, ws, a.disabled).await?;
    let installation = governance::installation_limits(&mut tx).await?;
    let k = match lineage {
        Some(lineage) => Some(governance::key_limits(&mut tx, ws, lineage).await?),
        None => None,
    };
    let catalog_override: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM workspace_catalog_overrides WHERE workspace_id=$1)",
    )
    .bind(ws)
    .fetch_one(&mut *tx)
    .await?;
    let type_catalogs: Value = sqlx::query_scalar("SELECT coalesce(jsonb_agg(jsonb_build_object('id',c.id,'name',c.name) ORDER BY c.name,c.id),'[]'::jsonb) FROM workspace_type_catalogs t JOIN catalogs c ON c.id=t.catalog_id WHERE t.kind=$1").bind(&l.kind).fetch_one(&mut *tx).await?;
    let override_catalogs: Value = sqlx::query_scalar("SELECT coalesce(jsonb_agg(jsonb_build_object('id',c.id,'name',c.name) ORDER BY c.name,c.id),'[]'::jsonb) FROM workspace_catalog_override_items i JOIN catalogs c ON c.id=i.catalog_id WHERE i.workspace_id=$1").bind(ws).fetch_one(&mut *tx).await?;
    // Budget reasons (workspace and key scopes only).
    let now: DateTime<Utc> = sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(&mut *tx)
        .await?;
    let platform_layer = if l.override_present {
        "workspace_override"
    } else {
        "type_default"
    };
    let mut global: Vec<Reason> = Vec::new();
    if a.disabled {
        // A disabled workspace refuses all new inference, whatever its configuration allows.
        global.push(Reason {
            code: "workspace_disabled",
            layer: "platform",
            period: None,
            blocking: true,
        });
    }
    let mut scopes: Vec<(&'static str, &governance::Limits, Option<Uuid>)> = vec![
        (platform_layer, &l.platform, None),
        ("workspace", &l.local, None),
    ];
    if let Some(k) = &k {
        scopes.push(("key", k, lineage));
    }
    for (layer, limits, lin) in &scopes {
        for (period, amount) in &limits.budgets {
            let (used, unresolved) = budget_consumption(&mut tx, ws, *lin, *period, now).await?;
            if unresolved {
                global.push(Reason {
                    code: "unresolved_usage_blocking",
                    layer,
                    period: Some(*period),
                    blocking: true,
                });
            } else if used.parse::<i128>().map_err(|_| invalid())? >= i128::from(*amount) {
                global.push(Reason {
                    code: "budget_exhausted",
                    layer,
                    period: Some(*period),
                    blocking: true,
                });
            }
        }
    }
    let eligible = "EXISTS(SELECT 1 FROM catalog_models cm WHERE cm.model_id=m.id AND ((EXISTS(SELECT 1 FROM workspace_catalog_overrides h WHERE h.workspace_id=w.id) AND EXISTS(SELECT 1 FROM workspace_catalog_override_items i WHERE i.workspace_id=w.id AND i.catalog_id=cm.catalog_id)) OR (NOT EXISTS(SELECT 1 FROM workspace_catalog_overrides h WHERE h.workspace_id=w.id) AND EXISTS(SELECT 1 FROM workspace_type_catalogs t WHERE t.kind=w.kind AND t.catalog_id=cm.catalog_id))))";
    let rows: Vec<ModelRow> = sqlx::query_as(&format!("SELECT m.id,m.public_name,m.display_name,m.enabled,{eligible},workspace_model_allowed(w.id,m.id) OR EXISTS(SELECT 1 FROM workspace_model_grants g WHERE g.workspace_id=w.id AND g.model_id=m.id),CASE WHEN $2::uuid IS NULL OR NOT EXISTS(SELECT 1 FROM key_model_restrictions r WHERE r.workspace_id=w.id AND r.governance_key_id=$2) THEN NULL ELSE EXISTS(SELECT 1 FROM key_model_selections s WHERE s.workspace_id=w.id AND s.governance_key_id=$2 AND s.model_id=m.id) END,(SELECT count(*) FROM deployments d WHERE d.model_id=m.id),(SELECT count(*) FROM deployments d WHERE d.model_id=m.id AND {}) FROM models m CROSS JOIN workspaces w WHERE w.id=$1 AND ({eligible} OR EXISTS(SELECT 1 FROM workspace_model_grants g WHERE g.workspace_id=w.id AND g.model_id=m.id) OR m.id=$3) ORDER BY m.public_name,m.id LIMIT 201", *resources::SERVING_ROUTE)).bind(ws).bind(lineage).bind(model).fetch_all(&mut *tx).await?;
    let restriction: Option<Value> = match lineage {
        Some(lin) => Some(sqlx::query_scalar("SELECT CASE WHEN EXISTS(SELECT 1 FROM key_model_restrictions WHERE workspace_id=$1 AND governance_key_id=$2) THEN jsonb_build_object('mode','restricted','model_ids',coalesce((SELECT jsonb_agg(model_id ORDER BY model_id) FROM key_model_selections WHERE workspace_id=$1 AND governance_key_id=$2),'[]'::jsonb)) ELSE jsonb_build_object('mode','inherit','model_ids',NULL) END").bind(ws).bind(lin).fetch_one(&mut *tx).await?),
        None => None,
    };
    let counts: (i64, i64) = sqlx::query_as("SELECT count(*) FILTER(WHERE source='catalog'),count(*) FILTER(WHERE source='direct') FROM workspace_model_grants WHERE workspace_id=$1").bind(ws).fetch_one(&mut *tx).await?;
    tx.commit().await?;
    let truncated = rows.len() > 200;
    let catalog_layer = if catalog_override {
        "workspace_override"
    } else {
        "type_default"
    };
    let mut models = Vec::new();
    let mut per_model: Vec<Vec<Reason>> = Vec::new();
    for (
        id,
        public_name,
        display_name,
        enabled,
        eligible,
        granted,
        key_selected,
        routes,
        enabled_routes,
    ) in rows.into_iter().take(200)
    {
        let mut reasons: Vec<Reason> = Vec::new();
        let mut push = |code, layer, blocking| {
            reasons.push(Reason {
                code,
                layer,
                period: None,
                blocking,
            })
        };
        if !enabled {
            push("model_disabled", "platform", true);
        }
        if enabled_routes == 0 {
            push("no_enabled_route", "platform", true);
        } else if enabled_routes < routes {
            push("some_routes_unavailable", "platform", false);
        }
        if !eligible && !granted {
            push("not_in_catalog", catalog_layer, true);
        } else if !granted {
            push("not_selected", "workspace", true);
        }
        if key_selected == Some(false) {
            push("key_restriction", "key", true);
        }
        reasons.extend(global.iter().cloned());
        let final_status = status(&reasons, LAYERS.len() - 1);
        models.push(json!({"model_id":id,"public_name":public_name,"display_name":display_name,"status":final_status,"reasons":reasons.iter().map(Reason::json).collect::<Vec<_>>()}));
        per_model.push(reasons);
    }
    let summary = |upto: usize| {
        let mut c = json!({"available":0,"partial":0,"unavailable":0});
        for reasons in &per_model {
            let s = status(reasons, upto);
            c[s] = json!(c[s].as_i64().unwrap_or(0) + 1);
        }
        c
    };
    let installation_visible = a.platform_reader;
    let layers = vec![
        json!({"layer":"platform","source":"installation","applies":true,"visible":installation_visible,"limits":installation_visible.then(||rates(&installation)),"budgets":installation_visible.then(||governance::json_budgets(&installation.budgets)),"catalogs":null,"models":summary(0)}),
        json!({"layer":"type_default","source":"type_default","applies":!l.override_present,"catalogs_apply":!catalog_override,"limits":rates(&l.type_default),"budgets":governance::json_budgets(&l.type_default.budgets),"catalogs":type_catalogs,"models":summary(1)}),
        json!({"layer":"workspace_override","source":"workspace_override","applies":l.override_present,"catalogs_apply":catalog_override,"limits":l.override_present.then(||rates(&l.platform)),"budgets":l.override_present.then(||governance::json_budgets(&l.platform.budgets)),"catalogs":catalog_override.then_some(override_catalogs),"models":summary(2)}),
        json!({"layer":"workspace","source":"local","applies":true,"limits":rates(&l.local),"budgets":governance::json_budgets(&l.local.budgets),"catalogs":null,"selections":{"catalog":counts.0,"direct":counts.1},"models":summary(3)}),
        match &k {
            Some(k) => {
                json!({"layer":"key","source":"key","applies":true,"limits":rates(k),"budgets":governance::json_budgets(&k.budgets),"catalogs":null,"restriction":restriction,"models":summary(4)})
            }
            None => {
                json!({"layer":"key","source":"key","applies":false,"limits":null,"budgets":null,"catalogs":null,"restriction":null,"models":summary(4)})
            }
        },
    ];
    Ok(Json(
        json!({"workspace_id":ws,"key_id":key,"workspace_disabled":a.disabled,"layers":layers,"summary":summary(4),"models":models,"truncated":truncated}),
    ))
}
pub(super) async fn workspace_access(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path(ws): Path<Uuid>,
    Query(q): Query<AccessQuery>,
) -> ApiResult {
    access(s, u, ws, None, q.model_id).await
}
pub(super) async fn key_access(
    State(s): State<Store>,
    Extension(u): Extension<BrowserPrincipal>,
    Path((ws, key)): Path<(Uuid, Uuid)>,
    Query(q): Query<AccessQuery>,
) -> ApiResult {
    access(s, u, ws, Some(key), q.model_id).await
}
