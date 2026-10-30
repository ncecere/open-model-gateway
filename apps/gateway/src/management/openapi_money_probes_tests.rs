//! Slice 4: explicit, handler-derived probes for inputs the derived HTTP vectors
//! cannot generate (multibyte limits, flips, caps with DISTINCT elements,
//! cross-field rules, boundary values, positional forms). Each probe compares
//! what the SCHEMA says with what the real handler answers over HTTP, and names
//! only a static label, the template and statuses on failure.
use super::vectors::{Vx, coherent_billing, meters};
use super::*;

/// How the handler is expected to answer (by status class), pinned per probe.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Handler {
    /// Any non-refusal (2xx, 3xx, 404, 409): only `Want::Agree` uses it. Pinned
    /// accept probes state `Status(200)` instead.
    Accept,
    Refuse,
    Status(u16),
}
impl Handler {
    fn holds(self, status: StatusCode) -> bool {
        match self {
            Self::Accept => matches!(status.as_u16(), 200..=399 | 404 | 409),
            Self::Refuse => matches!(status.as_u16(), 400 | 422),
            Self::Status(n) => status.as_u16() == n,
        }
    }
}
/// `Agree`: the handler's verdict must equal the schema's. `Pinned`: the
/// handler's answer and the schema's verdict are both stated, so a KNOWN
/// difference (a byte limit, a cross-field rule, a lenient parser) is recorded
/// and breaks the test the moment either side changes.
#[derive(Clone, Copy)]
enum Want {
    Agree,
    Pinned(Handler, bool),
}
fn mb(bytes: usize) -> String {
    "\u{e9}".repeat(bytes / 2)
}
fn unique() -> String {
    Uuid::new_v4().simple().to_string()
}
fn stamp(base: chrono::NaiveDateTime, hours: i64) -> String {
    (base + chrono::TimeDelta::hours(hours))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string()
}

impl Vx<'_> {
    fn judge(&self, label: &str, template: &str, want: Want, status: StatusCode, schema: bool) {
        let accepted = Handler::Accept.holds(status);
        let refused = Handler::Refuse.holds(status);
        let bad = match want {
            Want::Agree => !(accepted || refused) || accepted != schema,
            Want::Pinned(handler, expected) => !handler.holds(status) || schema != expected,
        };
        if bad {
            self.failures.borrow_mut().push(format!(
                "{template}: probe `{label}`: schema {} but the handler answered {}",
                if schema { "accepts" } else { "refuses" },
                status.as_u16()
            ));
        }
    }
    #[allow(clippy::too_many_arguments)]
    async fn probe(
        &self,
        label: &str,
        actor: &Browser,
        method: &str,
        template: &str,
        path: &str,
        body: Value,
        want: Want,
    ) {
        let schema = self.schema_body(method, template, &body);
        let captured = send(
            self.client,
            self.base,
            Some(actor),
            method,
            path,
            Some(body),
        )
        .await;
        self.judge(label, template, want, captured.status, schema);
    }
    #[allow(clippy::too_many_arguments)]
    async fn probe_query(
        &self,
        label: &str,
        actor: &Browser,
        template: &str,
        path: &str,
        query: String,
        param: (&str, String),
        want: Want,
    ) {
        let schema = self.schema_param(template, param.0, &param.1);
        let captured = send(
            self.client,
            self.base,
            Some(actor),
            "GET",
            &format!("{path}?{query}"),
            None,
        )
        .await;
        self.judge(label, template, want, captured.status, schema);
    }

    pub(super) async fn probes(&self) {
        self.guard("multibyte limits", self.multibyte()).await;
        self.guard("recipient caps", self.recipients()).await;
        self.guard("mapping rules", self.mapping_rules()).await;
        self.guard("mapping caps", self.mapping_caps()).await;
        self.guard("import caps", self.import_caps()).await;
        self.guard("import magnitudes", self.import_magnitudes())
            .await;
        self.guard("reconcile", self.reconcile_probes()).await;
        self.guard("holds cursor", self.cursor_probes()).await;
        self.guard("positional forms", self.positional_probes())
            .await;
    }

    fn base_mapping(&self, account: &str) -> Value {
        let day = chrono::Utc::now().date_naive() - chrono::TimeDelta::days(20);
        let mut mapping = provider_cost_tests::mapping(self.connection, Some(self.team));
        mapping["account_id"] = json!(account);
        mapping["bindings"][0]["effective_start"] = json!(format!("{day}T00:00:00Z"));
        mapping["bindings"][0]["effective_end"] =
            json!(format!("{}T00:00:00Z", day + chrono::TimeDelta::days(1)));
        mapping
    }

    // -----------------------------------------------------------------
    // UTF-8 byte limits: the handler counts bytes (valid_name, bounded(),
    // the report filters); the schema states them as x-omg-max-bytes (its
    // maxLength counts characters, #405). The probes prove it against the real
    // handler: a multibyte value of exactly the limit in bytes is accepted by
    // both, two bytes more is refused by both.
    // -----------------------------------------------------------------
    async fn multibyte(&self) {
        let centers = "/api/v1/platform/cost-centers";
        for (label, name, code, over) in [
            (
                "cost center name at 120 bytes",
                format!("{}{}", mb(88), unique()),
                format!("c-{}", unique()),
                false,
            ),
            (
                "cost center name at 122 bytes",
                format!("{}{}", mb(90), unique()),
                format!("c-{}", unique()),
                true,
            ),
            (
                "cost center code at 120 bytes",
                format!("n-{}", unique()),
                format!("{}{}", mb(88), unique()),
                false,
            ),
            (
                "cost center code at 122 bytes",
                format!("n-{}", unique()),
                format!("{}{}", mb(90), unique()),
                true,
            ),
        ] {
            let want = if over {
                Want::Pinned(Handler::Refuse, false)
            } else {
                Want::Pinned(Handler::Status(200), true)
            };
            self.probe(
                label,
                self.admin,
                "POST",
                centers,
                centers,
                json!({"name":name,"code":code}),
                want,
            )
            .await;
        }
        // Provider mapping text: 200 / 256 / 4096 / 512 bytes.
        let mappings = "/api/v1/platform/provider-cost-mappings";
        type Set = fn(&mut Value, String);
        let fields: [(&str, usize, Set); 5] = [
            ("mapping account_id", 200, |m, s| m["account_id"] = json!(s)),
            ("mapping evidence_reference", 256, |m, s| {
                m["evidence_reference"] = json!(s);
            }),
            ("binding evidence_reference", 256, |m, s| {
                m["bindings"][0]["evidence_reference"] = json!(s);
            }),
            ("binding provider_workspace_id", 4096, |m, s| {
                m["bindings"][0]["provider_workspace_id"] = json!(s);
            }),
            ("model mapping provider_model", 512, |m, s| {
                m["bindings"][0]["model_mappings"] =
                    json!([{"provider_model":s,"upstream_model":"upstream-model"}]);
            }),
        ];
        for (label, limit, set) in fields {
            for (extra, want) in [
                (0, Want::Pinned(Handler::Status(200), true)),
                (2, Want::Pinned(Handler::Refuse, false)),
            ] {
                let mut body = self.base_mapping(&format!("mb-{}", unique()));
                set(&mut body, mb(limit + extra));
                self.probe(
                    &format!("{label} at {} bytes", limit + extra),
                    self.admin,
                    "POST",
                    mappings,
                    mappings,
                    body,
                    want,
                )
                .await;
            }
        }
        // The upstream model counts bytes as well.
        for (extra, want) in [
            (0, Want::Pinned(Handler::Status(200), true)),
            (2, Want::Pinned(Handler::Refuse, false)),
        ] {
            let mut body = self.base_mapping(&format!("mb-{}", unique()));
            body["bindings"][0]["model_mappings"] =
                json!([{"provider_model":"provider-model","upstream_model":mb(512 + extra)}]);
            self.probe(
                &format!("model mapping upstream_model at {} bytes", 512 + extra),
                self.admin,
                "POST",
                mappings,
                mappings,
                body,
                want,
            )
            .await;
        }
        // Report filters: at most 200 bytes after percent-decoding.
        let template = "/api/v1/platform/cost-report";
        for name in ["model", "provider"] {
            for (characters, want) in [
                (100, Want::Pinned(Handler::Status(200), true)),
                (101, Want::Pinned(Handler::Refuse, false)),
            ] {
                self.probe_query(
                    &format!("{name} filter of {characters} two-byte characters"),
                    self.auditor,
                    template,
                    template,
                    format!(
                        "start_date={}&end_date={}&{name}={}",
                        self.start,
                        self.end,
                        "%C3%A9".repeat(characters)
                    ),
                    (name, "\u{e9}".repeat(characters)),
                    want,
                )
                .await;
            }
        }
    }

    // -----------------------------------------------------------------
    // recipient_user_ids: 50 DISTINCT eligible users are accepted and 51
    // are refused for the cap (not for duplicates or ineligibility), and
    // uniqueness compares parsed UUIDs while the schema compares strings.
    // -----------------------------------------------------------------
    async fn recipients(&self) {
        let mut ids = Vec::new();
        for n in 0..51 {
            let u = user(self.pool, &format!("recipient-{n}"), "auditor").await;
            member(self.pool, self.team, u.user_id, "admin").await;
            ids.push(u.user_id);
        }
        let wsbase = format!("/api/v1/workspaces/{}", self.team);
        let scopes = [
            (
                self.admin,
                "/api/v1/platform/report-schedules".to_owned(),
                "/api/v1/platform/report-schedules",
                format!(
                    "/api/v1/platform/report-schedules/{}",
                    self.platform_schedule
                ),
                "/api/v1/platform/report-schedules/{id}",
            ),
            (
                self.owner,
                format!("{wsbase}/report-schedules"),
                "/api/v1/workspaces/{ws}/report-schedules",
                format!("{wsbase}/report-schedules/{}", self.workspace_schedule),
                "/api/v1/workspaces/{ws}/report-schedules/{id}",
            ),
        ];
        for (actor, path, template, target, patch_template) in scopes {
            let create = |recipients: Value| json!({"cadence":"weekly","timezone":"UTC","delivery_time":"00:01","state":"paused","recipient_user_ids":recipients});
            let upper = ids[0].to_string().to_uppercase();
            for (label, recipients, want) in [
                (
                    "50 distinct eligible recipients",
                    json!(ids[..50]),
                    Want::Pinned(Handler::Status(200), true),
                ),
                (
                    "51 distinct eligible recipients",
                    json!(ids[..51]),
                    Want::Pinned(Handler::Refuse, false),
                ),
                (
                    "one UUID in two letter cases",
                    json!([ids[0], upper]),
                    Want::Pinned(Handler::Refuse, true),
                ),
                (
                    "two distinct recipients",
                    json!([ids[0], ids[1]]),
                    Want::Pinned(Handler::Status(200), true),
                ),
            ] {
                self.probe(
                    label,
                    actor,
                    "POST",
                    template,
                    &path,
                    create(recipients.clone()),
                    want,
                )
                .await;
                // The same lists on update (a PATCH needs no other field).
                self.probe(
                    label,
                    actor,
                    "PATCH",
                    patch_template,
                    &target,
                    json!({"recipient_user_ids":recipients}),
                    want,
                )
                .await;
            }
        }
    }

    // -----------------------------------------------------------------
    // Mapping rules: the flips (booleans are not derived by the shared
    // vectors), and the relations between bindings.
    // -----------------------------------------------------------------
    async fn mapping_rules(&self) {
        let mappings = "/api/v1/platform/provider-cost-mappings";
        let day = chrono::Utc::now().date_naive() - chrono::TimeDelta::days(20);
        let at = |n: i64| format!("{}T00:00:00Z", day + chrono::TimeDelta::days(n));
        let account = || format!("rule-{}", unique());
        type Edit = fn(&mut Value);
        let single: [(&str, Edit, Want); 6] = [
            (
                "attested=false",
                |m| m["bindings"][0]["attested"] = json!(false),
                Want::Pinned(Handler::Refuse, false),
            ),
            (
                "exclusive with non-OMG traffic",
                |m| m["bindings"][0]["non_omg_traffic"] = json!(true),
                Want::Pinned(Handler::Refuse, false),
            ),
            (
                "exclusive without a workspace",
                |m| m["bindings"][0]["workspace_id"] = Value::Null,
                Want::Pinned(Handler::Refuse, false),
            ),
            (
                "shared non-OMG binding without a workspace",
                |m| {
                    m["bindings"][0]["exclusive"] = json!(false);
                    m["bindings"][0]["non_omg_traffic"] = json!(true);
                    m["bindings"][0]["workspace_id"] = Value::Null;
                },
                Want::Pinned(Handler::Status(200), true),
            ),
            (
                "duplicate provider_model",
                |m| {
                    m["bindings"][0]["model_mappings"] = json!([
                        {"provider_model":"alias","upstream_model":"one"},
                        {"provider_model":"alias","upstream_model":"two"}
                    ]);
                },
                Want::Pinned(Handler::Refuse, true),
            ),
            (
                "distinct provider_models",
                |m| {
                    m["bindings"][0]["model_mappings"] = json!([
                        {"provider_model":"alias-a","upstream_model":"one"},
                        {"provider_model":"alias-b","upstream_model":"one"}
                    ]);
                },
                Want::Pinned(Handler::Status(200), true),
            ),
        ];
        for (label, edit, want) in single {
            let mut body = self.base_mapping(&account());
            edit(&mut body);
            self.probe(label, self.admin, "POST", mappings, mappings, body, want)
                .await;
        }
        // end must be after start (the schema states a format, not an order).
        let mut body = self.base_mapping(&account());
        body["bindings"][0]["effective_end"] = body["bindings"][0]["effective_start"].clone();
        self.probe(
            "window ending when it starts",
            self.admin,
            "POST",
            mappings,
            mappings,
            body,
            Want::Pinned(Handler::Refuse, true),
        )
        .await;
        // Two bindings: overlap and adjacency.
        let pair = |second: &dyn Fn(&mut Value)| {
            let mut body = self.base_mapping(&account());
            let mut next = body["bindings"][0].clone();
            second(&mut next);
            body["bindings"].as_array_mut().unwrap().push(next);
            body
        };
        let shared = |b: &mut Value| {
            b["exclusive"] = json!(false);
            b["non_omg_traffic"] = json!(true);
            b["workspace_id"] = Value::Null;
        };
        for (label, body, want) in [
            (
                "overlap, same provider workspace",
                pair(&|b| {
                    b["effective_start"] = json!(at(0));
                    b["effective_end"] = json!(at(2));
                }),
                Want::Pinned(Handler::Refuse, true),
            ),
            (
                "overlap, same connection, other provider workspace",
                pair(&|b| {
                    shared(b);
                    b["provider_workspace_id"] = json!("another-provider-workspace");
                }),
                Want::Pinned(Handler::Refuse, true),
            ),
            (
                "adjacent windows, same provider workspace",
                pair(&|b| {
                    b["effective_start"] = json!(at(1));
                    b["effective_end"] = json!(at(2));
                }),
                Want::Pinned(Handler::Status(200), true),
            ),
        ] {
            self.probe(label, self.admin, "POST", mappings, mappings, body, want)
                .await;
        }
    }

    /// 1000 bindings and 100 model mappings per binding: at the cap and one
    /// past it, with distinct, non-overlapping, otherwise valid elements.
    async fn mapping_caps(&self) {
        let mappings = "/api/v1/platform/provider-cost-mappings";
        let base = chrono::Utc::now().date_naive() - chrono::TimeDelta::days(60);
        let origin = base.and_hms_opt(0, 0, 0).unwrap();
        let binding = |n: i64| {
            json!({"provider_workspace_id":null,"connection_id":self.connection,"deployment_id":null,"workspace_id":null,
                "effective_start":stamp(origin, n),"effective_end":stamp(origin, n + 1),"exclusive":false,
                "non_omg_traffic":true,"attested":true,"evidence_reference":"cap:binding","model_mappings":[]})
        };
        for (count, want) in [
            (1000_i64, Want::Pinned(Handler::Status(200), true)),
            (1001, Want::Pinned(Handler::Refuse, false)),
        ] {
            let mut body = self.base_mapping(&format!("cap-{}", unique()));
            body["bindings"] = json!((0..count).map(binding).collect::<Vec<_>>());
            self.probe(
                &format!("{count} bindings"),
                self.admin,
                "POST",
                mappings,
                mappings,
                body,
                want,
            )
            .await;
        }
        for (count, want) in [
            (100, Want::Pinned(Handler::Status(200), true)),
            (101, Want::Pinned(Handler::Refuse, false)),
        ] {
            let mut body = self.base_mapping(&format!("cap-{}", unique()));
            body["bindings"][0]["model_mappings"] = json!(
                (0..count)
                    .map(|n| json!({"provider_model":format!("alias-{n}"),"upstream_model":"upstream"}))
                    .collect::<Vec<_>>()
            );
            self.probe(
                &format!("{count} model mappings"),
                self.admin,
                "POST",
                mappings,
                mappings,
                body,
                want,
            )
            .await;
        }
    }

    /// 31 pages and 31 buckets per page: at the cap with an intact chain, and
    /// one past it. (A 32-bucket chain also spans more than 31 days, so the
    /// refusal past the cap cannot be isolated from the span rule here; the
    /// cap itself is the schema's maxItems.)
    /// A chain of `days` one-day buckets (the first holds one item with the
    /// given amount), `per_page` buckets to a page, from a fresh account.
    fn import_chain(&self, days: usize, per_page: usize, amount: &str) -> Value {
        let start = chrono::Utc::now().date_naive() - chrono::TimeDelta::days(45);
        let origin = start.and_hms_opt(0, 0, 0).unwrap();
        let item = json!({"amount":amount,"currency":"USD","workspace_id":null,"description":null,"model":null,"service_tier":null,"context_window":null,"cost_type":null,"token_type":null,"inference_geo":null,"speed":null});
        let buckets: Vec<Value> = (0..days as i64)
            .map(|d| {
                json!({"starting_at":stamp(origin, d * 24),"ending_at":stamp(origin, d * 24 + 24),
                    "results": if d == 0 { vec![item.clone()] } else { vec![] }})
            })
            .collect();
        let chunks: Vec<&[Value]> = buckets.chunks(per_page).collect();
        let pages: Vec<Value> = chunks
            .iter()
            .enumerate()
            .map(|(i, data)| {
                let more = i + 1 < chunks.len();
                json!({"data":data,"has_more":more,"next_page":more.then(|| format!("cursor-{i}"))})
            })
            .collect();
        let end = stamp(origin, days as i64 * 24);
        json!({"account_id":format!("cap-{}", unique()),"evidence_reference":"cap:export","exported_at":end,"supersedes":null,
            "request":{"starting_at":stamp(origin, 0),"ending_at":end,"bucket_width":"1d","group_by":["workspace_id","description"],"limit":per_page.min(31)},
            "pages":pages})
    }
    async fn import_caps(&self) {
        let imports = "/api/v1/platform/provider-cost-imports";
        let chain = |days: usize, per_page: usize| self.import_chain(days, per_page, "1");
        for (label, days, per_page, want) in [
            (
                "31 pages of one bucket",
                31,
                1,
                Want::Pinned(Handler::Status(200), true),
            ),
            (
                "32 pages of one bucket",
                32,
                1,
                Want::Pinned(Handler::Refuse, false),
            ),
            (
                "31 buckets in one page",
                31,
                31,
                Want::Pinned(Handler::Status(200), true),
            ),
            (
                "32 buckets in one page",
                32,
                32,
                Want::Pinned(Handler::Refuse, false),
            ),
        ] {
            self.probe(
                label,
                self.admin,
                "POST",
                imports,
                imports,
                chain(days, per_page),
                want,
            )
            .await;
        }
    }

    /// Amount magnitude over HTTP (the DB-free unit test
    /// `management_openapi_money_amount_magnitude_matches_cents` covers the
    /// exact boundary): 34 whole
    /// digits convert to micro-USD within i128 and fit numeric(39,0); 35 nines
    /// exceed the limit and are refused.
    async fn import_magnitudes(&self) {
        let imports = "/api/v1/platform/provider-cost-imports";
        for (label, amount, want) in [
            (
                "amount of 34 whole digits",
                "9".repeat(34),
                Want::Pinned(Handler::Status(200), true),
            ),
            (
                "amount of 35 whole digits above the limit",
                "9".repeat(35),
                Want::Pinned(Handler::Refuse, false),
            ),
        ] {
            self.probe(
                label,
                self.admin,
                "POST",
                imports,
                imports,
                self.import_chain(1, 1, &amount),
                want,
            )
            .await;
        }
    }

    // -----------------------------------------------------------------
    // Reconcile: optional siblings, i64 boundaries, the leading-zero
    // leniency, coherence rules, and characters-not-bytes evidence. Every
    // request consumes its own fresh hold.
    // -----------------------------------------------------------------
    async fn reconcile_probe(&self, label: &str, body: Value, want: Want) {
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
        self.probe(
            label,
            self.admin,
            "POST",
            "/api/v1/workspaces/{ws}/costs/{execution}/reconcile",
            &format!("/api/v1/workspaces/{}/costs/{hold}/reconcile", self.team),
            body,
            want,
        )
        .await;
    }
    async fn reconcile_probes(&self) {
        let full = || json!({"input_tokens":"3","output_tokens":"2","billing_usage":coherent_billing(),"evidence":"probe:receipt","meter_usage":meters(),"output_image_variant":"1024x1024","provider_cost_microusd":"5"});
        let bare = |input: &str, output: &str| json!({"input_tokens":input,"output_tokens":output,"billing_usage":null,"evidence":"probe:receipt"});
        let billing = |set: &[(&str, &str)]| {
            let mut value = json!({"total_input_tokens":null,"uncached_input_tokens":null,"cache_read_input_tokens":null,"cache_write_input_tokens":null,"cache_write_default_input_tokens":null,"cache_write_5m_input_tokens":null,"cache_write_1h_input_tokens":null});
            for (name, count) in set {
                value[*name] = json!(count);
            }
            let mut body = bare("3", "2");
            body["billing_usage"] = value;
            body
        };
        const MAX: &str = "9223372036854775807";
        const OVER: &str = "9223372036854775808";
        // The accept side is strict: exactly 200 (Handler::Accept would also
        // pass a 404 or a 409).
        let accept = Want::Pinned(Handler::Status(200), true);
        let refuse = Want::Pinned(Handler::Refuse, false);
        // Current behaviour; see #408 (a Storage error maps to 503).
        let incoherent = Want::Pinned(Handler::Status(503), true);
        self.reconcile_probe("every optional sibling", full(), accept)
            .await;
        self.reconcile_probe("input_tokens at i64::MAX", bare(MAX, "0"), accept)
            .await;
        self.reconcile_probe("input_tokens past i64::MAX", bare(OVER, "0"), refuse)
            .await;
        self.reconcile_probe("output_tokens at i64::MAX", bare("0", MAX), accept)
            .await;
        self.reconcile_probe("output_tokens past i64::MAX", bare("0", OVER), refuse)
            .await;
        for (label, value, want) in [
            ("provider_cost_microusd at i64::MAX", MAX, accept),
            ("provider_cost_microusd past i64::MAX", OVER, refuse),
            // The handler's money() tolerates zero padding that the contract
            // (canonical decimals) does not describe: pinned, not hidden.
            (
                "zero-padded provider_cost_microusd",
                "007",
                Want::Pinned(Handler::Status(200), false),
            ),
        ] {
            let mut body = full();
            body["provider_cost_microusd"] = json!(value);
            self.reconcile_probe(label, body, want).await;
        }
        self.reconcile_probe(
            "zero-padded input_tokens",
            bare("007", "2"),
            Want::Pinned(Handler::Status(200), false),
        )
        .await;
        // Coherent usage is accepted.
        for (label, set) in [
            (
                "total equals uncached + read + write",
                vec![
                    ("total_input_tokens", "10"),
                    ("uncached_input_tokens", "4"),
                    ("cache_read_input_tokens", "6"),
                    ("cache_write_input_tokens", "0"),
                ],
            ),
            (
                "write parts below the aggregate, total given",
                vec![
                    ("total_input_tokens", "5"),
                    ("cache_write_input_tokens", "5"),
                    ("cache_write_default_input_tokens", "2"),
                    ("cache_write_5m_input_tokens", "3"),
                ],
            ),
        ] {
            self.reconcile_probe(label, billing(&set), accept).await;
        }
        // Coherent, but the hold is pinned to a version-1 price, which values
        // the input from billing_usage.total_input_tokens: without it there is
        // no complete pinned valuation (409).
        self.reconcile_probe(
            "coherent usage without a total on a version-1 price",
            billing(&[
                ("cache_write_input_tokens", "5"),
                ("cache_write_default_input_tokens", "2"),
                ("cache_write_5m_input_tokens", "3"),
            ]),
            Want::Pinned(Handler::Status(409), true),
        )
        .await;
        // BillingUsage::validate refuses these, but the refusal surfaces as a
        // Storage error: 503 "Reconciliation unavailable" (usage_values in
        // src/governance.rs -> the error mapping of reconcile in
        // src/management/governance/reports.rs), not 400. The schema cannot
        // state the cross-field rules, so it accepts them. Current behaviour;
        // see #408.
        self.reconcile_probe(
            "input_tokens + output_tokens past i64",
            bare(MAX, "1"),
            incoherent,
        )
        .await;
        for (label, set) in [
            (
                "uncached above total",
                vec![("total_input_tokens", "1"), ("uncached_input_tokens", "5")],
            ),
            (
                "write parts above the aggregate",
                vec![
                    ("cache_write_input_tokens", "1"),
                    ("cache_write_default_input_tokens", "2"),
                ],
            ),
            (
                "write parts all given and not summing to the aggregate",
                vec![
                    ("cache_write_input_tokens", "5"),
                    ("cache_write_default_input_tokens", "1"),
                    ("cache_write_5m_input_tokens", "1"),
                    ("cache_write_1h_input_tokens", "1"),
                ],
            ),
            (
                "total not equal to the full breakdown",
                vec![
                    ("total_input_tokens", "10"),
                    ("uncached_input_tokens", "1"),
                    ("cache_read_input_tokens", "1"),
                    ("cache_write_input_tokens", "1"),
                ],
            ),
            (
                "input sum past i64",
                vec![
                    ("uncached_input_tokens", MAX),
                    ("cache_read_input_tokens", "1"),
                ],
            ),
            (
                "write parts sum past i64",
                vec![
                    ("cache_write_default_input_tokens", MAX),
                    ("cache_write_5m_input_tokens", "1"),
                ],
            ),
            (
                "raw input above the billing total",
                vec![("total_input_tokens", "1")],
            ),
        ] {
            let mut body = billing(&set);
            if label == "raw input above the billing total" {
                body["input_tokens"] = json!("5");
            }
            self.reconcile_probe(label, body, incoherent).await;
        }
        // Evidence counts characters (chars().count() <= 200), not bytes.
        for (characters, want) in [(200, accept), (201, refuse)] {
            let mut body = bare("3", "2");
            body["evidence"] = json!("\u{e9}".repeat(characters));
            self.reconcile_probe(
                &format!("evidence of {characters} two-byte characters"),
                body,
                want,
            )
            .await;
        }
    }

    // -----------------------------------------------------------------
    // Holds cursor: the emitted spelling is the only accepted one, and a
    // timestamp before PostgreSQL's timestamptz range is refused
    // (pg_timestamptz_representable in src/management/governance/holds.rs), as
    // is a 19-digit value outside the representable range (cursor_parts in
    // src/management/logs.rs: i64 parse, then from_timestamp_micros); the
    // cursor pattern expresses neither.
    // -----------------------------------------------------------------
    async fn cursor_probes(&self) {
        let nil = Uuid::nil();
        let wsbase = format!("/api/v1/workspaces/{}", self.team);
        for (actor, template, path) in [
            (
                self.auditor,
                "/api/v1/platform/holds",
                "/api/v1/platform/holds".to_owned(),
            ),
            (
                self.owner,
                "/api/v1/workspaces/{ws}/holds",
                format!("{wsbase}/holds"),
            ),
        ] {
            for (label, micros, want) in [
                (
                    "cursor at the earliest timestamptz",
                    "-210866803200000000",
                    Want::Pinned(Handler::Status(200), true),
                ),
                (
                    "cursor before the earliest timestamptz",
                    "-210866803200000001",
                    Want::Pinned(Handler::Refuse, true),
                ),
                // 19 digits that do not parse as an i64 (cursor_parts).
                (
                    "cursor of 19 digits past i64",
                    "9999999999999999999",
                    Want::Pinned(Handler::Refuse, true),
                ),
                // An i64 that from_timestamp_micros cannot represent.
                (
                    "cursor of i64::MAX microseconds",
                    "9223372036854775807",
                    Want::Pinned(Handler::Refuse, true),
                ),
            ] {
                let cursor = format!("{micros}_{nil}");
                self.probe_query(
                    label,
                    actor,
                    template,
                    &path,
                    format!("cursor={cursor}"),
                    ("cursor", cursor),
                    want,
                )
                .await;
            }
        }
    }

    // -----------------------------------------------------------------
    // Positional forms: serde also reads each DTO from an array in field
    // order, and the schema documents that form. Send the valid array and
    // mutated arrays (a wrong value at each position, one element more, one
    // element fewer) to the real handler; its verdict must equal the schema's.
    // -----------------------------------------------------------------
    async fn positional(
        &self,
        actor: &Browser,
        method: &str,
        template: &str,
        keys: &[&str],
        prepare: impl AsyncFn() -> (String, Value),
    ) {
        type Mutation = Box<dyn Fn(Vec<Value>) -> Vec<Value>>;
        let mut cases: Vec<(String, Mutation)> = vec![("valid".into(), Box::new(|a| a))];
        for index in 0..keys.len() {
            cases.push((
                format!("wrong value at position {index}"),
                Box::new(move |mut a| {
                    a[index] = json!({"wrong":true});
                    a
                }),
            ));
        }
        cases.push((
            "one element more".into(),
            Box::new(|mut a| {
                a.push(json!(1));
                a
            }),
        ));
        cases.push((
            "one element fewer".into(),
            Box::new(|mut a| {
                a.pop();
                a
            }),
        ));
        for (label, mutate) in cases {
            let (path, object) = prepare().await;
            let array: Vec<Value> = keys.iter().map(|k| object[*k].clone()).collect();
            self.probe(
                &label,
                actor,
                method,
                template,
                &path,
                Value::Array(mutate(array)),
                Want::Agree,
            )
            .await;
        }
    }
    async fn positional_probes(&self) {
        let wsbase = format!("/api/v1/workspaces/{}", self.team);
        let centers = "/api/v1/platform/cost-centers";
        let fresh_center =
            || json!({"name":format!("pos {}", unique()),"code":format!("pos-{}", unique())});
        self.positional(self.admin, "POST", centers, &["name", "code"], async || {
            (centers.to_owned(), fresh_center())
        })
        .await;
        self.positional(
            self.admin,
            "PATCH",
            "/api/v1/platform/cost-centers/{id}",
            &["name", "code"],
            async || {
                let id = Uuid::new_v4();
                sqlx::query(
                    "INSERT INTO cost_centers(id,name,code) VALUES($1,'Positional target',$2)",
                )
                .bind(id)
                .bind(format!("target-{}", unique()))
                .execute(self.pool)
                .await
                .unwrap();
                (format!("{centers}/{id}"), fresh_center())
            },
        )
        .await;
        let create = |recipient: Uuid| json!({"cadence":"weekly","timezone":"UTC","delivery_time":"00:01","state":"paused","cost_center_id":null,"recipient_user_ids":[recipient]});
        let update = |recipient: Uuid| json!({"cadence":"weekly","timezone":"UTC","delivery_time":"00:02","state":"paused","recipient_user_ids":[recipient]});
        for (actor, scope, template, recipient, schedule) in [
            (
                self.admin,
                "/api/v1/platform".to_owned(),
                "/api/v1/platform/report-schedules",
                self.f.admin.user_id,
                self.platform_schedule,
            ),
            (
                self.owner,
                wsbase.clone(),
                "/api/v1/workspaces/{ws}/report-schedules",
                self.f.owner.user_id,
                self.workspace_schedule,
            ),
        ] {
            self.positional(
                actor,
                "POST",
                template,
                &[
                    "cadence",
                    "timezone",
                    "delivery_time",
                    "state",
                    "cost_center_id",
                    "recipient_user_ids",
                ],
                async || (format!("{scope}/report-schedules"), create(recipient)),
            )
            .await;
            let patch_template = format!("{template}/{{id}}");
            let target = format!("{scope}/report-schedules/{schedule}");
            self.positional(
                actor,
                "PATCH",
                &patch_template,
                &[
                    "cadence",
                    "timezone",
                    "delivery_time",
                    "state",
                    "recipient_user_ids",
                ],
                async || (target.clone(), update(recipient)),
            )
            .await;
        }
        let imports = "/api/v1/platform/provider-cost-imports";
        let day = chrono::Utc::now().date_naive() - chrono::TimeDelta::days(25);
        let upload = || {
            let mut upload = provider_cost_tests::upload();
            upload["account_id"] = json!("positional-account");
            upload["request"]["starting_at"] = json!(format!("{day}T00:00:00Z"));
            upload["request"]["ending_at"] =
                json!(format!("{}T00:00:00Z", day + chrono::TimeDelta::days(1)));
            upload["exported_at"] = upload["request"]["ending_at"].clone();
            upload["pages"][0]["data"][0]["starting_at"] = upload["request"]["starting_at"].clone();
            upload["pages"][0]["data"][0]["ending_at"] = upload["request"]["ending_at"].clone();
            upload
        };
        self.positional(
            self.admin,
            "POST",
            imports,
            &[
                "account_id",
                "evidence_reference",
                "exported_at",
                "supersedes",
                "request",
                "pages",
            ],
            async || (imports.to_owned(), upload()),
        )
        .await;
        let mappings = "/api/v1/platform/provider-cost-mappings";
        self.positional(
            self.admin,
            "POST",
            mappings,
            &["account_id", "supersedes", "evidence_reference", "bindings"],
            async || {
                (
                    mappings.to_owned(),
                    self.base_mapping(&format!("positional-{}", unique())),
                )
            },
        )
        .await;
        let recon = "/api/v1/platform/provider-reconciliations";
        self.positional(
            self.admin,
            "POST",
            recon,
            &["import_id", "mapping_id"],
            async || {
                (
                    recon.to_owned(),
                    json!({"import_id":self.import,"mapping_id":self.mapping}),
                )
            },
        )
        .await;
        self.positional(
            self.admin,
            "POST",
            "/api/v1/workspaces/{ws}/costs/{execution}/reconcile",
            &[
                "input_tokens",
                "output_tokens",
                "billing_usage",
                "evidence",
                "meter_usage",
                "output_image_variant",
                "provider_cost_microusd",
            ],
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
                (
                    format!("{wsbase}/costs/{hold}/reconcile"),
                    json!({"input_tokens":"3","output_tokens":"2","billing_usage":coherent_billing(),"evidence":"positional:receipt","meter_usage":meters(),"output_image_variant":"1024x1024","provider_cost_microusd":"5"}),
                )
            },
        )
        .await;
    }
}
