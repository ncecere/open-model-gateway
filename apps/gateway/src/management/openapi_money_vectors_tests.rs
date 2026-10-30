//! HTTP boundary vectors for slice 4: every documented query parameter and
//! JSON request body is compared, schema versus the real handler, over HTTP.
use super::*;
use futures::FutureExt;
use openapi::http_vectors::{
    Probe, body_vectors_http, query_vectors_extended, query_vectors_fixed,
};

pub(super) struct Vx<'a> {
    pub client: &'a reqwest::Client,
    pub base: &'a str,
    pub pool: &'a PgPool,
    pub f: &'a Fixture,
    pub admin: &'a Browser,
    pub auditor: &'a Browser,
    pub owner: &'a Browser,
    pub start: String,
    pub end: String,
    pub team: Uuid,
    pub cc: Uuid,
    pub key: Uuid,
    pub account: Uuid,
    pub model_id: Uuid,
    pub deployment: Uuid,
    pub price: Uuid,
    pub connection: Uuid,
    pub import: Uuid,
    pub mapping: Uuid,
    pub platform_schedule: Uuid,
    pub workspace_schedule: Uuid,
    pub failures: std::cell::RefCell<Vec<String>>,
}

/// The artifact's verdict on a JSON request body of an operation.
impl Vx<'_> {
    pub(super) fn schema_body(&self, method: &str, template: &str, body: &Value) -> bool {
        let document = openapi::responses::artifact();
        let schema = &document["paths"][template][method.to_ascii_lowercase()]["requestBody"]["content"]
            ["application/json"]["schema"];
        openapi::http_vectors::accepts(document, schema, body).unwrap()
    }
    /// The artifact's verdict on one query parameter value.
    pub(super) fn schema_param(&self, template: &str, name: &str, raw: &str) -> bool {
        let document = openapi::responses::artifact();
        let parameter = document["paths"][template]["get"]["parameters"]
            .as_array()
            .and_then(|all| all.iter().find(|p| p["name"] == name))
            .unwrap();
        openapi::http_vectors::accepts(document, &parameter["schema"], &json!(raw)).unwrap()
    }
}

/// A binding instant exactly as the handler's serde reads it
/// (`provider_costs.rs` Binding: `DateTime<Utc>`); anything else is a decode
/// refusal, not a relation.
fn instant(value: &Value) -> Option<chrono::DateTime<chrono::Utc>> {
    serde_json::from_value(value.clone()).ok()
}
/// create_mapping()'s relations between elements, recomputed from the body:
/// a window must end after it starts, a binding's provider models are unique,
/// and two bindings may not overlap in time when they share a provider
/// workspace or a connection (a missing deployment overlaps every deployment).
fn mapping_conflicts(input: &Value) -> bool {
    let Some(bindings) = input["bindings"].as_array() else {
        return false;
    };
    for (index, binding) in bindings.iter().enumerate() {
        let (Some(start), Some(end)) = (
            instant(&binding["effective_start"]),
            instant(&binding["effective_end"]),
        ) else {
            return false;
        };
        if end <= start {
            return true;
        }
        let mut names = std::collections::BTreeSet::new();
        for mapped in binding["model_mappings"].as_array().into_iter().flatten() {
            if !names.insert(
                mapped["provider_model"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned(),
            ) {
                return true;
            }
        }
        for previous in &bindings[..index] {
            let (Some(before_start), Some(before_end)) = (
                instant(&previous["effective_start"]),
                instant(&previous["effective_end"]),
            ) else {
                return false;
            };
            if start < before_end
                && end > before_start
                && (binding["provider_workspace_id"] == previous["provider_workspace_id"]
                    || (binding["connection_id"] == previous["connection_id"]
                        && (binding["deployment_id"].is_null()
                            || previous["deployment_id"].is_null()
                            || binding["deployment_id"] == previous["deployment_id"])))
            {
                return true;
            }
        }
    }
    false
}
/// A `billing_usage` that BillingUsage::validate accepts: total 3 = uncached 3
/// + cache read 0 + cache writes 0 (the three parts sum to the aggregate).
pub(super) fn coherent_billing() -> Value {
    json!({"total_input_tokens":"3","uncached_input_tokens":"3","cache_read_input_tokens":"0","cache_write_input_tokens":"0","cache_write_default_input_tokens":"0","cache_write_5m_input_tokens":"0","cache_write_1h_input_tokens":"0"})
}
/// Meter evidence: two known counters, the rest unknown (never zero).
pub(super) fn meters() -> Value {
    json!({"output_images":"1","input_characters":null,"input_audio_seconds_ms":null,"output_audio_seconds_ms":null,"search_units":null,"requests":"1"})
}

fn pairs(list: &[(&str, String)]) -> Vec<(String, String)> {
    list.iter()
        .map(|(n, v)| ((*n).to_owned(), v.clone()))
        .collect()
}
fn param(query: &str, name: &str) -> Option<String> {
    query
        .split('&')
        .filter_map(|p| p.split_once('='))
        .find(|(n, _)| *n == name)
        .map(|(_, v)| v.to_owned())
}

impl Vx<'_> {
    /// Record a harness refusal (its message names method, template, vector
    /// kind and status only) instead of aborting the remaining operations.
    pub(super) async fn guard(&self, label: &str, fut: impl std::future::Future<Output = ()>) {
        if let Err(payload) = std::panic::AssertUnwindSafe(fut).catch_unwind().await {
            let message = payload
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| payload.downcast_ref::<&str>().map(|s| (*s).to_owned()))
                .unwrap_or_default();
            self.failures
                .borrow_mut()
                .push(format!("{label}: {message}"));
        }
    }
    async fn query(
        &self,
        seen: &mut PositiveFixtures,
        actor: &Browser,
        template: &str,
        path: &str,
        base: Vec<(&str, String)>,
    ) {
        let valid = pairs(&base);
        self.guard(
            template,
            query_vectors_extended(
                seen,
                "GET",
                template,
                &[],
                // The status list is a comma-separated pattern with no length
                // bound; one state and the same state with a trailing comma
                // straddle its boundary.
                if valid.iter().any(|(name, _)| name == "status") {
                    &[("status", "succeeded"), ("status", "succeeded,")]
                } else {
                    &[]
                },
                async || ((), valid.clone()),
                async |_: &(), query: String| {
                    let captured = send(
                        self.client,
                        self.base,
                        Some(actor),
                        "GET",
                        &format!("{path}?{query}"),
                        None,
                    )
                    .await;
                    // `then_by` must differ from `group_by`: a rule between two
                    // parameters that per-parameter schemas cannot state (it is in
                    // the parameter description), so only that refusal is decided.
                    let same = param(&query, "group_by")
                        .zip(param(&query, "then_by"))
                        .is_some_and(|(a, b)| a == b);
                    if same && captured.status == StatusCode::BAD_REQUEST {
                        Probe::decided(captured, true)
                    } else {
                        Probe::from(captured)
                    }
                },
            ),
        )
        .await;
    }

    /// Operations whose valid request is a fixed query.
    async fn query_fixed(
        &self,
        seen: &mut PositiveFixtures,
        actor: &Browser,
        template: &str,
        path: &str,
        base: &[(&str, &str)],
    ) {
        self.guard(
            template,
            query_vectors_fixed(seen, "GET", template, base, async |query: String| {
                send(
                    self.client,
                    self.base,
                    Some(actor),
                    "GET",
                    &format!("{path}?{query}"),
                    None,
                )
                .await
            }),
        )
        .await;
    }

    fn report_filters(&self, workspace: Uuid) -> Vec<(&'static str, String)> {
        vec![
            ("start_date", self.start.clone()),
            ("end_date", self.end.clone()),
            ("compare", "none".into()),
            ("workspace_id", workspace.to_string()),
            ("actor_user_id", self.f.owner.user_id.to_string()),
            ("service_account_id", self.account.to_string()),
            ("key_id", self.key.to_string()),
            ("status", "succeeded".into()),
            ("cost_center_id", self.cc.to_string()),
            ("model", "money-contract".into()),
            ("provider", "anthropic".into()),
            ("accounting_status", "settled".into()),
        ]
    }
    fn usage_filters(&self, workspace: Uuid) -> Vec<(&'static str, String)> {
        vec![
            ("start_date", self.start.clone()),
            ("end_date", self.end.clone()),
            ("workspace_id", workspace.to_string()),
            ("model_id", self.model_id.to_string()),
            ("key_id", self.key.to_string()),
            ("member_user_id", self.f.owner.user_id.to_string()),
            ("service_account_id", self.account.to_string()),
            ("status", "succeeded".into()),
            ("cost_center_id", self.cc.to_string()),
        ]
    }

    pub(super) async fn queries(&self, seen: &mut PositiveFixtures) {
        let ws = self.team;
        let wsbase = format!("/api/v1/workspaces/{ws}");
        let page = || vec![("limit", "10".to_owned()), ("offset", "0".to_owned())];
        // 19 digits: the longest cursor the schema allows (56 characters) is also a
        // timestamp within timestamptz's range, so the vectors can start from it.
        let cursor = format!("1700000000000000000_{}", Uuid::nil());
        let holds = || vec![("limit", "10".to_owned()), ("cursor", cursor.clone())];
        self.query(
            seen,
            self.auditor,
            "/api/v1/platform/cost-centers",
            "/api/v1/platform/cost-centers",
            page(),
        )
        .await;
        self.query(
            seen,
            self.auditor,
            "/api/v1/platform/cost-report",
            "/api/v1/platform/cost-report",
            self.report_filters(ws),
        )
        .await;
        self.query(
            seen,
            self.owner,
            "/api/v1/workspaces/{ws}/cost-report",
            &format!("{wsbase}/cost-report"),
            self.report_filters(ws),
        )
        .await;
        let mut costs = self.report_filters(ws);
        costs.extend([("limit", "10".to_owned()), ("offset", "0".to_owned())]);
        self.query(
            seen,
            self.owner,
            "/api/v1/workspaces/{ws}/costs",
            &format!("{wsbase}/costs"),
            costs.clone(),
        )
        .await;
        self.query(
            seen,
            self.owner,
            "/api/v1/workspaces/{ws}/usage-export",
            &format!("{wsbase}/usage-export"),
            costs,
        )
        .await;
        let mut explore = self.usage_filters(ws);
        explore.extend([
            ("metric", "spend".to_owned()),
            ("group_by", "model".to_owned()),
            ("then_by", "provider".to_owned()),
            ("top", "5".to_owned()),
        ]);
        self.query(
            seen,
            self.auditor,
            "/api/v1/platform/usage/explore",
            "/api/v1/platform/usage/explore",
            explore.clone(),
        )
        .await;
        self.query(
            seen,
            self.owner,
            "/api/v1/workspaces/{ws}/usage/explore",
            &format!("{wsbase}/usage/explore"),
            explore,
        )
        .await;
        self.query(
            seen,
            self.auditor,
            "/api/v1/platform/usage/overview",
            "/api/v1/platform/usage/overview",
            self.usage_filters(ws),
        )
        .await;
        self.query(
            seen,
            self.owner,
            "/api/v1/workspaces/{ws}/usage/overview",
            &format!("{wsbase}/usage/overview"),
            self.usage_filters(ws),
        )
        .await;
        let fixed_dates = [
            ("start_date", self.start.as_str()),
            ("end_date", self.end.as_str()),
        ];
        self.query_fixed(
            seen,
            self.auditor,
            "/api/v1/platform/usage/storage",
            "/api/v1/platform/usage/storage",
            &fixed_dates,
        )
        .await;
        self.query_fixed(
            seen,
            self.owner,
            "/api/v1/workspaces/{ws}/usage/storage",
            &format!("{wsbase}/usage/storage"),
            &fixed_dates,
        )
        .await;
        self.query(
            seen,
            self.auditor,
            "/api/v1/platform/holds",
            "/api/v1/platform/holds",
            holds(),
        )
        .await;
        self.query(
            seen,
            self.owner,
            "/api/v1/workspaces/{ws}/holds",
            &format!("{wsbase}/holds"),
            holds(),
        )
        .await;
        let schedules = |limit: &str| {
            vec![
                ("limit", limit.to_owned()),
                ("after", Uuid::nil().to_string()),
            ]
        };
        self.query(
            seen,
            self.admin,
            "/api/v1/platform/report-schedules",
            "/api/v1/platform/report-schedules",
            schedules("10"),
        )
        .await;
        self.query(
            seen,
            self.owner,
            "/api/v1/workspaces/{ws}/report-schedules",
            &format!("{wsbase}/report-schedules"),
            schedules("10"),
        )
        .await;
        for (actor, template, path) in [
            (
                self.admin,
                "/api/v1/platform/report-schedules/{id}/runs",
                format!(
                    "/api/v1/platform/report-schedules/{}/runs",
                    self.platform_schedule
                ),
            ),
            (
                self.owner,
                "/api/v1/workspaces/{ws}/report-schedules/{id}/runs",
                format!("{wsbase}/report-schedules/{}/runs", self.workspace_schedule),
            ),
        ] {
            self.query(
                seen,
                actor,
                template,
                &path,
                vec![("limit", "10".to_owned())],
            )
            .await;
        }
        let imports = "/api/v1/platform/provider-cost-imports";
        self.query(seen, self.auditor, imports, imports, page())
            .await;
        self.query(
            seen,
            self.auditor,
            "/api/v1/platform/provider-cost-imports/{id}/lines",
            &format!("{imports}/{}/lines", self.import),
            page(),
        )
        .await;
        self.query(
            seen,
            self.auditor,
            "/api/v1/platform/provider-cost-mappings",
            "/api/v1/platform/provider-cost-mappings",
            page(),
        )
        .await;
        self.query(
            seen,
            self.auditor,
            "/api/v1/platform/provider-reconciliations",
            "/api/v1/platform/provider-reconciliations",
            page(),
        )
        .await;
    }

    /// Run the HTTP body vectors once per wire form: every `valid` body (an
    /// object, the serde positional array, or a mix of nested forms) is walked
    /// as its own run, so each declared union arm is carried by one of them.
    /// `decide` reads the handler's answer with the ORIGINAL input in hand.
    #[allow(clippy::too_many_arguments)]
    async fn forms(
        &self,
        seen: &mut PositiveFixtures,
        actor: &Browser,
        method: &str,
        template: &str,
        runs: Vec<Value>,
        path: impl AsyncFn() -> String,
        decide: impl AsyncFn(Captured, Value) -> Probe,
    ) {
        for valid in runs {
            self.guard(
                template,
                body_vectors_http(seen, method, template, valid, async |input: Value| {
                    let target = path().await;
                    let captured = send(
                        self.client,
                        self.base,
                        Some(actor),
                        method,
                        &target,
                        Some(input.clone()),
                    )
                    .await;
                    decide(captured, input).await
                }),
            )
            .await;
        }
    }

    pub(super) async fn bodies(&self, seen: &mut PositiveFixtures) {
        let platform = "/api/v1/platform";
        let wsbase = format!("/api/v1/workspaces/{}", self.team);
        let plain = async |captured: Captured, _: Value| Probe::from(captured);
        // Cost centers.
        let centers = "/api/v1/platform/cost-centers";
        let center = json!({"name":"Vector label","code":"VECTOR"});
        self.forms(
            seen,
            self.admin,
            "POST",
            centers,
            vec![center.clone(), seq(&center, COST_CENTER)],
            async || centers.to_owned(),
            plain,
        )
        .await;
        let target = Uuid::new_v4();
        sqlx::query("INSERT INTO cost_centers(id,name,code) VALUES($1,'Patch target','PATCHME')")
            .bind(target)
            .execute(self.pool)
            .await
            .unwrap();
        let patched = json!({"name":"Patched label","code":"PATCHED"});
        self.forms(
            seen,
            self.admin,
            "PATCH",
            "/api/v1/platform/cost-centers/{id}",
            vec![patched.clone(), seq(&patched, COST_CENTER)],
            async || format!("{platform}/cost-centers/{target}"),
            plain,
        )
        .await;
        // Report schedules, both scopes. A paused schedule is the PATCH target.
        for (actor, scope, template, recipient) in [
            (
                self.admin,
                platform.to_owned(),
                "/api/v1/platform/report-schedules",
                self.f.admin.user_id,
            ),
            (
                self.owner,
                wsbase.clone(),
                "/api/v1/workspaces/{ws}/report-schedules",
                self.f.owner.user_id,
            ),
        ] {
            let platform_scope = scope == platform;
            let create = json!({"cadence":"weekly","timezone":"UTC","delivery_time":"00:01","state":"paused","cost_center_id":self.cc,"recipient_user_ids":[recipient]});
            let path = format!("{scope}/report-schedules");
            let known_zone = |body: &Value| {
                body["timezone"]
                    .as_str()
                    .is_none_or(|zone| jiff::tz::TimeZone::get(zone).is_ok())
            };
            self.forms(
                seen,
                actor,
                "POST",
                template,
                vec![create.clone(), seq(&create, SCHEDULE_CREATE)],
                async || path.clone(),
                async |captured, input| {
                    self.schedule_verdict(
                        captured,
                        self.schema_body("POST", template, &input),
                        view(&input, SCHEDULE_CREATE),
                        platform_scope,
                        &known_zone,
                    )
                    .await
                },
            )
            .await;
            let created = id(&body(
                &send(
                    self.client,
                    self.base,
                    Some(actor),
                    "POST",
                    &path,
                    Some(create),
                )
                .await,
            ));
            let target = format!("{path}/{created}");
            let patch = json!({"cadence":"monthly","timezone":"UTC","delivery_time":"00:05","state":"paused","recipient_user_ids":[recipient]});
            let patch_template = format!("{template}/{{id}}");
            self.forms(
                seen,
                actor,
                "PATCH",
                &patch_template,
                vec![patch.clone(), seq(&patch, SCHEDULE_UPDATE)],
                async || target.clone(),
                async |captured, input| {
                    self.schedule_verdict(
                        captured,
                        self.schema_body("PATCH", &patch_template, &input),
                        view(&input, SCHEDULE_UPDATE),
                        platform_scope,
                        &known_zone,
                    )
                    .await
                },
            )
            .await;
        }
        // Provider evidence. A chain of pages or buckets must be contiguous and
        // linked by has_more/next_page, which per-element schemas cannot state.
        // The refusal is decided only when THIS body really breaks the chain
        // (the parser's own rules, recomputed from the body) and the schema
        // accepts it; a refusal of an intact chain, or any other status, stands.
        let imports = "/api/v1/platform/provider-cost-imports";
        let runs: Vec<Value> = [(false, false), (false, true), (true, false), (true, true)]
            .into_iter()
            .enumerate()
            .map(|(n, (root, request))| upload_valid(root, request, n))
            .collect();
        self.forms(
            seen,
            self.admin,
            "POST",
            imports,
            runs,
            async || imports.to_owned(),
            async |captured, input| {
                if !provider_costs::parser::upload_parses(&input)
                    && captured.status == StatusCode::BAD_REQUEST
                    && self.schema_body("POST", imports, &input)
                {
                    Probe::decided(captured, true)
                } else {
                    Probe::from(captured)
                }
            },
        )
        .await;
        // Bindings may not overlap in time or ownership, end after they start,
        // and a binding's provider models are unique: relations between
        // elements that a schema cannot state. Decided only when this body
        // really violates one of them (create_mapping's rules, recomputed).
        let mappings = "/api/v1/platform/provider-cost-mappings";
        let runs: Vec<Value> = [false, true]
            .into_iter()
            .enumerate()
            .map(|(n, root)| self.mapping_valid(root, n))
            .collect();
        self.forms(
            seen,
            self.admin,
            "POST",
            mappings,
            runs,
            async || mappings.to_owned(),
            async |captured, input| {
                if mapping_conflicts(&mapping_view(&input))
                    && captured.status == StatusCode::BAD_REQUEST
                    && self.schema_body("POST", mappings, &input)
                {
                    Probe::decided(captured, true)
                } else {
                    Probe::from(captured)
                }
            },
        )
        .await;
        let recon = "/api/v1/platform/provider-reconciliations";
        let pair = json!({"import_id":self.import,"mapping_id":self.mapping});
        self.forms(
            seen,
            self.admin,
            "POST",
            recon,
            vec![pair.clone(), seq(&pair, RECONCILIATION)],
            async || recon.to_owned(),
            plain,
        )
        .await;
        // Reconciliation consumes its hold, so every request gets a fresh one.
        // billing_usage is null: BillingUsage's cross-field rules refuse an
        // incoherent counter with 503 (#408), so no counter can be varied on
        // its own (the schema's billing arm carries the reviewed exemption and
        // the pinned probes cover the counters). The meter evidence, the image
        // variant and the provider cost carry every optional sibling, in each
        // wire form of the root and of the meter evidence.
        let meter = json!({"output_images":"1","input_characters":"2","input_audio_seconds_ms":"3","output_audio_seconds_ms":"4","search_units":"5","requests":"6","output_video_seconds_ms":"7"});
        let reconcile = |meter: Value| json!({"input_tokens":"1","output_tokens":"1","billing_usage":null,"evidence":"vector:receipt","meter_usage":meter,"output_image_variant":"1024x1024","provider_cost_microusd":"5"});
        let mut runs = Vec::new();
        for root in [false, true] {
            for meter in [meter.clone(), seq(&meter, METER), Value::Null] {
                let object = reconcile(meter);
                runs.push(if root {
                    seq(&object, RECONCILE)
                } else {
                    object
                });
            }
        }
        self.forms(
            seen,
            self.admin,
            "POST",
            "/api/v1/workspaces/{ws}/costs/{execution}/reconcile",
            runs,
            async || {
                let hold = seed_attempt(
                    self.pool,
                    self.team,
                    self.key,
                    self.deployment,
                    self.price,
                    false,
                    3,
                )
                .await;
                format!("{wsbase}/costs/{hold}/reconcile")
            },
            plain,
        )
        .await;
    }

    /// The handler's verdict on a report schedule body. Two refusals are DOMAIN
    /// refusals a schema cannot state, and are decided only when the real state
    /// proves them and the schema accepts the body: an unknown time-zone, and a
    /// recipient who is not eligible for the scope (the handler's own count,
    /// recomputed from the database). Every other status stands.
    async fn schedule_verdict(
        &self,
        captured: Captured,
        schema_accepts: bool,
        object: Value,
        platform: bool,
        known_zone: &impl Fn(&Value) -> bool,
    ) -> Probe {
        // A body the schema refuses never earns a domain verdict: the handler's
        // input checks (for example more than 50 recipients) refuse it first.
        if !schema_accepts {
            return Probe::from(captured);
        }
        let ineligible = if captured.status == StatusCode::BAD_REQUEST {
            let recipients = object["recipient_user_ids"]
                .as_array()
                .map(|ids| {
                    ids.iter()
                        .map(|id| id.as_str().and_then(|id| id.parse::<Uuid>().ok()))
                        .collect::<Option<Vec<_>>>()
                })
                .unwrap_or_default();
            match recipients {
                Some(recipients) => {
                    let eligible: i64 = sqlx::query_scalar("SELECT count(*) FROM effective_platform_roles p WHERE p.user_id=ANY($1) AND ((p.role IN ('admin','auditor') AND $2) OR ($2 IS FALSE AND EXISTS(SELECT FROM effective_workspace_memberships m WHERE m.workspace_id=$3 AND m.user_id=p.user_id AND m.role IN ('owner','admin'))))")
                        .bind(&recipients)
                        .bind(platform)
                        .bind(self.team)
                        .fetch_one(self.pool)
                        .await
                        .unwrap();
                    eligible != recipients.len() as i64
                }
                None => false,
            }
        } else {
            false
        };
        if ineligible {
            return Probe::decided(captured, true);
        }
        zone_verdict(captured, &object, known_zone(&object))
    }
    /// A mapping that satisfies create_mapping, carrying every binding form
    /// (object, positional non-exclusive, positional exclusive) and both forms
    /// of model mapping in each, on its own days so no run overlaps another.
    fn mapping_valid(&self, positional: bool, run: usize) -> Value {
        let first =
            chrono::Utc::now().date_naive() - chrono::TimeDelta::days(200 + 10 * run as i64);
        let day = |n: i64| format!("{}T00:00:00Z", first + chrono::TimeDelta::days(n));
        let models = |n: usize| {
            let first = json!({"provider_model":format!("provider-model-{run}-{n}"),"upstream_model":"upstream-model"});
            let second = json!({"provider_model":format!("provider-model-{run}-{}", n + 1),"upstream_model":"upstream-model"});
            json!([first, seq(&second, MODEL_MAPPING)])
        };
        let binding = |n: i64, exclusive: bool| {
            json!({"provider_workspace_id":format!("provider-workspace-{run}-{n}"),"connection_id":self.connection,"deployment_id":null,
                "workspace_id":if exclusive { json!(self.team) } else { Value::Null },
                "effective_start":day(n),"effective_end":day(n + 1),"exclusive":exclusive,"non_omg_traffic":false,
                "attested":true,"evidence_reference":format!("finance:binding-{run}-{n}"),"model_mappings":models(2 * n as usize)})
        };
        let mapping = json!({"account_id":format!("vector-account-{run}"),"supersedes":null,"evidence_reference":"finance:mapping",
            "bindings":[binding(0, false), seq(&binding(1, false), BINDING), seq(&binding(2, true), BINDING)]});
        if positional {
            seq(&mapping, MAPPING)
        } else {
            mapping
        }
    }
}

// Serde field order of every DTO that also accepts a positional array (the
// schema documents both wire forms).
const COST_CENTER: &[&str] = &["name", "code"];
const SCHEDULE_CREATE: &[&str] = &[
    "cadence",
    "timezone",
    "delivery_time",
    "state",
    "cost_center_id",
    "recipient_user_ids",
];
const SCHEDULE_UPDATE: &[&str] = &[
    "cadence",
    "timezone",
    "delivery_time",
    "state",
    "recipient_user_ids",
];
const UPLOAD: &[&str] = &[
    "account_id",
    "evidence_reference",
    "exported_at",
    "supersedes",
    "request",
    "pages",
];
const PARAMETERS: &[&str] = &[
    "starting_at",
    "ending_at",
    "bucket_width",
    "group_by",
    "limit",
];
const PAGE: &[&str] = &["data", "has_more", "next_page"];
const BUCKET: &[&str] = &["starting_at", "ending_at", "results"];
const ITEM: &[&str] = &[
    "amount",
    "currency",
    "workspace_id",
    "description",
    "model",
    "service_tier",
    "context_window",
    "cost_type",
    "token_type",
    "inference_geo",
    "speed",
];
const MAPPING: &[&str] = &["account_id", "supersedes", "evidence_reference", "bindings"];
const BINDING: &[&str] = &[
    "provider_workspace_id",
    "connection_id",
    "deployment_id",
    "workspace_id",
    "effective_start",
    "effective_end",
    "exclusive",
    "non_omg_traffic",
    "attested",
    "evidence_reference",
    "model_mappings",
];
const MODEL_MAPPING: &[&str] = &["provider_model", "upstream_model"];
const RECONCILIATION: &[&str] = &["import_id", "mapping_id"];
const RECONCILE: &[&str] = &[
    "input_tokens",
    "output_tokens",
    "billing_usage",
    "evidence",
    "meter_usage",
    "output_image_variant",
    "provider_cost_microusd",
];
const METER: &[&str] = &[
    "output_images",
    "input_characters",
    "input_audio_seconds_ms",
    "output_audio_seconds_ms",
    "search_units",
    "requests",
    "output_video_seconds_ms",
];

/// The serde positional (sequence) wire form of an object: its values in
/// field order.
fn seq(object: &Value, keys: &[&str]) -> Value {
    Value::Array(keys.iter().map(|key| object[*key].clone()).collect())
}
/// The object view of a value that may be a positional array.
fn view(value: &Value, keys: &[&str]) -> Value {
    match value {
        Value::Array(items) => Value::Object(
            keys.iter()
                .zip(items)
                .map(|(key, item)| ((*key).to_owned(), item.clone()))
                .collect(),
        ),
        other => other.clone(),
    }
}
/// Every binding and model mapping of a mapping as objects.
fn mapping_view(input: &Value) -> Value {
    let mut mapping = view(input, MAPPING);
    if let Some(bindings) = mapping
        .as_object_mut()
        .and_then(|m| m.get_mut("bindings"))
        .and_then(Value::as_array_mut)
    {
        for binding in bindings {
            *binding = view(binding, BINDING);
            if let Some(models) = binding
                .as_object_mut()
                .and_then(|b| b.get_mut("model_mappings"))
                .and_then(Value::as_array_mut)
            {
                for model in models {
                    *model = view(model, MODEL_MAPPING);
                }
            }
        }
    }
    mapping
}
/// An RFC 3339 instant of EXACTLY 40 characters (the handler's limit): a day at
/// midnight with 19 fractional-second digits. Every value the vectors start from
/// is at the limit, so the at-limit and one-past-the-limit vectors are the value
/// itself and the value plus one character.
fn instant40(day: chrono::NaiveDate) -> String {
    format!("{day}T00:00:00.0000000000000000000Z")
}
/// An upload carrying every wire form of every nesting level: three pages (an
/// object page, a positional continuing page and a positional last page), each
/// of two consecutive one-day buckets (an object and a positional one), each of
/// two lines (an object and a positional one). `root` and `request` choose the
/// wire form of the upload itself and of its request parameters. Every instant
/// is 40 characters and every amount 64 (the handler's limits).
fn upload_valid(root: bool, request: bool, run: usize) -> Value {
    let first = chrono::Utc::now().date_naive() - chrono::TimeDelta::days(300 + 10 * run as i64);
    let line = |n: usize| {
        json!({"amount":format!("{}1.25", "0".repeat(60)),"currency":"USD","workspace_id":"wrkspc-vector",
            "description":format!("Vector line {run}-{n}"),"model":"claude-vector","service_tier":"standard",
            "context_window":"0-200k","cost_type":"tokens","token_type":"uncached_input_tokens",
            "inference_geo":"global","speed":"standard"})
    };
    let bucket = |n: i64| {
        let object = json!({"starting_at":instant40(first + chrono::TimeDelta::days(n)),
            "ending_at":instant40(first + chrono::TimeDelta::days(n + 1)),
            "results":[line(2 * n as usize), seq(&line(2 * n as usize + 1), ITEM)]});
        if n % 2 == 0 {
            object
        } else {
            seq(&object, BUCKET)
        }
    };
    let page = |n: i64, positional: bool, last: bool| {
        let object = json!({"data":[bucket(2 * n), bucket(2 * n + 1)],"has_more":!last,
            "next_page":if last { Value::Null } else { json!(format!("cursor-{run}-{n}")) }});
        if positional {
            seq(&object, PAGE)
        } else {
            object
        }
    };
    let parameters = json!({"starting_at":instant40(first),"ending_at":instant40(first + chrono::TimeDelta::days(6)),
        "bucket_width":"1d","group_by":["workspace_id","description"],"limit":31});
    let upload = json!({"account_id":format!("vector-account-{run}"),"evidence_reference":"finance:vector-export",
        "exported_at":instant40(first + chrono::TimeDelta::days(6)),"supersedes":null,
        "request":if request { seq(&parameters, PARAMETERS) } else { parameters },
        "pages":[page(0, false, false), page(1, true, false), page(2, true, true)]});
    if root { seq(&upload, UPLOAD) } else { upload }
}

/// The time-zone database is not enumerable in a schema: a well-formed but
/// unknown zone is refused by the handler although the schema (length 1..64)
/// accepts it. Only that refusal is decided; every other status stands. The
/// handler measures the zone in UTF-8 BYTES (`timezone.len() > 64`, #405), so
/// a multibyte zone longer than 64 bytes is NOT decided here: it stays a
/// plain refusal that the schema (which counts characters) would have to match.
fn zone_verdict(captured: Captured, input: &Value, known: bool) -> Probe {
    let length_ok = input["timezone"]
        .as_str()
        .is_some_and(|z| (1..=64).contains(&z.len()));
    if !known && length_ok && captured.status == StatusCode::BAD_REQUEST {
        Probe::decided(captured, true)
    } else {
        Probe::from(captured)
    }
}
