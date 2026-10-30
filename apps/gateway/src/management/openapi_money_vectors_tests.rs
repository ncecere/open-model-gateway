//! HTTP boundary vectors for slice 4: every documented query parameter and
//! JSON request body is compared, schema versus the real handler, over HTTP.
use super::*;
use futures::FutureExt;
use openapi::http_vectors::{Probe, body_vectors_http, query_vectors, query_vectors_fixed};

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
        openapi::responses::validate_value(document, schema, body).is_ok()
    }
    /// The artifact's verdict on one query parameter value.
    pub(super) fn schema_param(&self, template: &str, name: &str, raw: &str) -> bool {
        let document = openapi::responses::artifact();
        let parameter = document["paths"][template]["get"]["parameters"]
            .as_array()
            .and_then(|all| all.iter().find(|p| p["name"] == name))
            .unwrap();
        openapi::responses::validate_value(document, &parameter["schema"], &json!(raw)).is_ok()
    }
}

fn instant(v: &Value) -> Option<chrono::DateTime<chrono::Utc>> {
    v.as_str()?.parse().ok()
}
/// parse()'s rules for the page chain, recomputed from the body: every page
/// but the last has has_more=true and a distinct cursor, the last has
/// has_more=false and no cursor, and the buckets are consecutive one-day
/// buckets from request.starting_at to request.ending_at.
fn chain_broken(input: &Value) -> bool {
    let (Some(start), Some(end)) = (
        instant(&input["request"]["starting_at"]),
        instant(&input["request"]["ending_at"]),
    ) else {
        return false;
    };
    let Some(pages) = input["pages"].as_array() else {
        return false;
    };
    let mut expected = start;
    let mut cursors = std::collections::BTreeSet::new();
    for (index, page) in pages.iter().enumerate() {
        let last = index + 1 == pages.len();
        if page["has_more"].as_bool() == Some(last) {
            return true;
        }
        if !last
            && !page["next_page"]
                .as_str()
                .is_some_and(|s| cursors.insert(s.to_owned()))
        {
            return true;
        }
        if last && !page["next_page"].is_null() {
            return true;
        }
        for bucket in page["data"].as_array().into_iter().flatten() {
            let (Some(from), Some(to)) = (
                instant(&bucket["starting_at"]),
                instant(&bucket["ending_at"]),
            ) else {
                return false;
            };
            if from != expected || to != from + chrono::TimeDelta::days(1) || to > end {
                return true;
            }
            expected = to;
        }
    }
    expected != end
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
            query_vectors(
                seen,
                "GET",
                template,
                &[],
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
        let cursor = format!("1700000000000000_{}", Uuid::nil());
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

    async fn body(
        &self,
        seen: &mut PositiveFixtures,
        actor: &Browser,
        method: &str,
        template: &str,
        path: &str,
        valid: Value,
    ) {
        self.guard(
            template,
            body_vectors_http(seen, method, template, valid, async |input: Value| {
                Probe::from(
                    send(
                        self.client,
                        self.base,
                        Some(actor),
                        method,
                        path,
                        Some(input),
                    )
                    .await,
                )
            }),
        )
        .await;
    }

    pub(super) async fn bodies(&self, seen: &mut PositiveFixtures) {
        let platform = "/api/v1/platform";
        let wsbase = format!("/api/v1/workspaces/{}", self.team);
        // Cost centers.
        self.body(
            seen,
            self.admin,
            "POST",
            "/api/v1/platform/cost-centers",
            "/api/v1/platform/cost-centers",
            json!({"name":"Vector label","code":"VECTOR"}),
        )
        .await;
        let target = Uuid::new_v4();
        sqlx::query("INSERT INTO cost_centers(id,name,code) VALUES($1,'Patch target','PATCHME')")
            .bind(target)
            .execute(self.pool)
            .await
            .unwrap();
        self.body(
            seen,
            self.admin,
            "PATCH",
            "/api/v1/platform/cost-centers/{id}",
            &format!("{platform}/cost-centers/{target}"),
            json!({"name":"Patched label","code":"PATCHED"}),
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
            let create = json!({"cadence":"weekly","timezone":"UTC","delivery_time":"00:01","state":"paused","cost_center_id":self.cc,"recipient_user_ids":[recipient]});
            let path = format!("{scope}/report-schedules");
            let known_zone = |body: &Value| {
                body["timezone"]
                    .as_str()
                    .is_none_or(|zone| jiff::tz::TimeZone::get(zone).is_ok())
            };
            self.guard(
                template,
                body_vectors_http(
                    seen,
                    "POST",
                    template,
                    create.clone(),
                    async |input: Value| {
                        let captured = send(
                            self.client,
                            self.base,
                            Some(actor),
                            "POST",
                            &path,
                            Some(input.clone()),
                        )
                        .await;
                        zone_verdict(captured, &input, known_zone(&input))
                    },
                ),
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
            self.guard(
                &patch_template,
                body_vectors_http(
                    seen,
                    "PATCH",
                    &patch_template,
                    patch.clone(),
                    async |input: Value| {
                        let captured = send(
                            self.client,
                            self.base,
                            Some(actor),
                            "PATCH",
                            &target,
                            Some(input.clone()),
                        )
                        .await;
                        zone_verdict(captured, &input, known_zone(&input))
                    },
                ),
            )
            .await;
        }
        // Provider evidence.
        let imports = "/api/v1/platform/provider-cost-imports";
        let day = chrono::Utc::now().date_naive() - chrono::TimeDelta::days(20);
        let mut upload = provider_cost_tests::upload();
        upload["account_id"] = json!("vector-account");
        upload["request"]["starting_at"] = json!(format!("{day}T00:00:00Z"));
        upload["request"]["ending_at"] =
            json!(format!("{}T00:00:00Z", day + chrono::TimeDelta::days(1)));
        upload["exported_at"] = json!(format!("{}T00:00:00Z", day + chrono::TimeDelta::days(1)));
        upload["pages"][0]["data"][0]["starting_at"] = upload["request"]["starting_at"].clone();
        upload["pages"][0]["data"][0]["ending_at"] = upload["request"]["ending_at"].clone();
        // A chain of pages or buckets must be contiguous and linked by
        // has_more/next_page, which per-element schemas cannot state. The
        // refusal is decided only when THIS body really breaks the chain (the
        // parser's own rules, recomputed from the body) and the schema accepts
        // it; a refusal of an intact chain, or any other status, stands.
        self.guard(
            imports,
            body_vectors_http(seen, "POST", imports, upload, async |input: Value| {
                let captured = send(
                    self.client,
                    self.base,
                    Some(self.admin),
                    "POST",
                    imports,
                    Some(input.clone()),
                )
                .await;
                if chain_broken(&input)
                    && captured.status == StatusCode::BAD_REQUEST
                    && self.schema_body("POST", imports, &input)
                {
                    Probe::decided(captured, true)
                } else {
                    Probe::from(captured)
                }
            }),
        )
        .await;
        let mappings = "/api/v1/platform/provider-cost-mappings";
        let mut mapping = provider_cost_tests::mapping(self.connection, Some(self.team));
        mapping["account_id"] = json!("vector-account");
        mapping["bindings"][0]["effective_start"] = json!(format!("{day}T00:00:00Z"));
        mapping["bindings"][0]["effective_end"] =
            json!(format!("{}T00:00:00Z", day + chrono::TimeDelta::days(1)));
        // Bindings may not overlap in time or ownership, end after they start,
        // and a binding's provider models are unique: relations between
        // elements that a schema cannot state. Decided only when this body
        // really violates one of them (create_mapping's rules, recomputed).
        self.guard(
            mappings,
            body_vectors_http(seen, "POST", mappings, mapping, async |input: Value| {
                let captured = send(
                    self.client,
                    self.base,
                    Some(self.admin),
                    "POST",
                    mappings,
                    Some(input.clone()),
                )
                .await;
                if mapping_conflicts(&input)
                    && captured.status == StatusCode::BAD_REQUEST
                    && self.schema_body("POST", mappings, &input)
                {
                    Probe::decided(captured, true)
                } else {
                    Probe::from(captured)
                }
            }),
        )
        .await;
        let recon = "/api/v1/platform/provider-reconciliations";
        self.body(
            seen,
            self.admin,
            "POST",
            recon,
            recon,
            json!({"import_id":self.import,"mapping_id":self.mapping}),
        )
        .await;
        // Reconciliation consumes its hold, so every request gets a fresh one.
        // The valid base carries every optional sibling (meter evidence, image
        // variant, provider cost) and a coherent billing_usage, so the nested
        // fields are walked too.
        let valid = json!({"input_tokens":"3","output_tokens":"2","billing_usage":coherent_billing(),"evidence":"vector:receipt","meter_usage":meters(),"output_image_variant":"1024x1024","provider_cost_microusd":"5"});
        self.guard(
            "reconcile",
            body_vectors_http(
                seen,
                "POST",
                "/api/v1/workspaces/{ws}/costs/{execution}/reconcile",
                valid,
                async |input: Value| {
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
                    Probe::from(
                        send(
                            self.client,
                            self.base,
                            Some(self.admin),
                            "POST",
                            &format!("{wsbase}/costs/{hold}/reconcile"),
                            Some(input),
                        )
                        .await,
                    )
                },
            ),
        )
        .await;
    }
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
