//! Slice 3's executed, raw-wire registry: a real gateway and disposable PostgreSQL.
use super::*;
use openapi::http_vectors::{self, Probe};
use openapi::responses::SLICE3_AREAS;

struct Journey<'a> {
    client: reqwest::Client,
    base: String,
    actor: &'a Browser,
    observed: PositiveFixtures,
}
impl Journey<'_> {
    async fn request(&mut self, method: &str, path: &str, input: Option<Value>) -> Value {
        let r = send(
            &self.client,
            &self.base,
            Some(self.actor),
            method,
            path,
            input,
        )
        .await;
        assert!(r.status.is_success(), "{method} {path}: {}", r.status);
        self.observed.observe(&r);
        let value: Value = serde_json::from_slice(&r.bytes).unwrap();
        if let Some(data) = value.get("data") {
            assert!(
                !data.as_array().unwrap().is_empty(),
                "populated fixture {path}"
            );
        }
        assert!(!value.to_string().contains("env:CONTRACT_REFERENCE"));
        value
    }
}
fn legacy_price() -> Value {
    json!({"input_microusd_per_million":"9007199254740993","output_microusd_per_million":"1","input_token_limit":1,"output_token_limit":1})
}
fn v3_price() -> Value {
    let mut lines = vec![
        json!({"meter":"input_tokens","microusd_per_batch":"123","batch":1000000,"unit_label":"/M tokens","sku_label":"Input"}),
        json!({"meter":"output_tokens","microusd_per_batch":"456","batch":1000000,"unit_label":"/M tokens","sku_label":"Output"}),
    ];
    for meter in [
        "cache_read_tokens",
        "cache_write_tokens",
        "cache_write_5m_tokens",
        "cache_write_1h_tokens",
        "output_images",
        "input_characters",
        "input_audio_seconds_ms",
        "output_audio_seconds_ms",
        "search_units",
        "requests",
    ] {
        lines.push(json!({"meter":meter,"not_applicable":true}));
    }
    json!({"pricing_version":3,"input_token_limit":100,"output_token_limit":10,"price_lines":lines,"max_units":{"requests":"1"}})
}
/// Every meter priced in one version 3 price, as object or positional lines, with
/// the time-based meters at one billing unit (`tier`: per second, minute, hour)
/// and a ceiling for every capped meter: one price per arm family of the price
/// line union (a meter takes one line per variant and tier key).
fn full_price(tier: usize, positional: bool) -> Value {
    let time = [
        (1_000_u64, "/second"),
        (60_000, "/minute"),
        (3_600_000, "/hour"),
    ][tier];
    let tokens = (1_000_000_u64, "/M tokens");
    let meters: [(&str, (u64, &str)); 16] = [
        ("input_tokens", tokens),
        ("output_tokens", tokens),
        ("cache_read_tokens", tokens),
        ("cache_write_tokens", tokens),
        ("cache_write_5m_tokens", tokens),
        ("cache_write_1h_tokens", tokens),
        ("output_images", (1, "/image")),
        ("input_characters", (1_000_000, "/M characters")),
        ("input_audio_seconds_ms", time),
        ("output_audio_seconds_ms", time),
        ("search_units", (1, "/search")),
        ("requests", (1, "/request")),
        ("input_audio_tokens", tokens),
        ("cache_read_audio_tokens", tokens),
        ("output_audio_tokens", tokens),
        ("output_video_seconds_ms", time),
    ];
    let lines: Vec<Value> = meters
        .iter()
        .map(|(meter, (batch, unit))| {
            if positional {
                json!([meter, "1", batch, unit, "Rate", null, null, false, false])
            } else {
                json!({"meter":meter,"microusd_per_batch":"1","batch":batch,"unit_label":unit,"sku_label":"Rate"})
            }
        })
        .collect();
    json!({"pricing_version":3,"input_token_limit":100,"output_token_limit":10,"price_lines":lines,
        "max_units":{"output_images":"1","input_characters":"1","input_audio_seconds_ms":"1","output_audio_seconds_ms":"1","search_units":"1","requests":"1","output_video_seconds_ms":"1"}})
}
/// [`v3_price`] with every line in the positional form (all nine items, so the
/// 9-item bound is the line itself), plus a positional explicit unknown line.
fn positional_baseline() -> Value {
    let mut body = v3_price();
    let mut lines: Vec<Value> = body["price_lines"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| {
            if l["not_applicable"] == true {
                json!([l["meter"], null, null, null, null, null, null, true, false])
            } else {
                json!([
                    l["meter"],
                    l["microusd_per_batch"],
                    l["batch"],
                    l["unit_label"],
                    l["sku_label"],
                    null,
                    null,
                    false,
                    false
                ])
            }
        })
        .collect();
    lines.push(json!([
        "input_audio_tokens",
        null,
        null,
        null,
        null,
        null,
        null,
        false,
        true
    ]));
    body["price_lines"] = json!(lines);
    body
}
/// Every price shape the vectors walk: the baseline, its positional form, each
/// arm family of the price-line union (object and positional), version 1 and 2.
fn price_variants() -> Vec<Value> {
    let mut out = vec![v3_price(), positional_baseline()];
    for tier in 0..3 {
        for positional in [false, true] {
            out.push(full_price(tier, positional));
        }
    }
    out
}
/// The twelve meters every price must state (`Meter::ALL`).
const METERS: [&str; 12] = [
    "input_tokens",
    "output_tokens",
    "cache_read_tokens",
    "cache_write_tokens",
    "cache_write_5m_tokens",
    "cache_write_1h_tokens",
    "output_images",
    "input_characters",
    "input_audio_seconds_ms",
    "output_audio_seconds_ms",
    "search_units",
    "requests",
];
/// A priced line: `(meter, batch)` fixes the canonical unit label.
fn line(meter: &str, batch: u64, amount: u64) -> Value {
    let label = match (meter, batch) {
        ("output_images", _) => "/image",
        ("input_characters", _) => "/M characters",
        ("search_units", _) => "/search",
        ("requests", _) => "/request",
        (_, 1000) => "/second",
        (_, 60000) => "/minute",
        (_, 3600000) => "/hour",
        _ => "/M tokens",
    };
    json!({"meter":meter,"microusd_per_batch":amount.to_string(),"batch":batch,"unit_label":label,"sku_label":format!("SKU {meter}")})
}
fn tiered(mut l: Value, min: u64) -> Value {
    l["min_prompt_tokens"] = json!(min);
    l
}
fn varied(mut l: Value, variant: &str) -> Value {
    l["variant"] = json!(variant);
    l
}
/// A v3 price stating all twelve meters: the given priced lines, the rest not
/// applicable. The batch list mirrors the same meters at other amounts.
fn v3_with(priced: &[Value], extra: &[Value], max_units: Value) -> Value {
    let build = |bump: u64| {
        let mut lines: Vec<Value> = METERS
            .iter()
            .filter(|m| !priced.iter().any(|p| p["meter"] == **m))
            .map(|m| json!({"meter":m,"not_applicable":true}))
            .collect();
        for l in priced.iter().chain(extra) {
            let mut l = l.clone();
            let amount: u64 = l["microusd_per_batch"].as_str().unwrap().parse().unwrap();
            l["microusd_per_batch"] = json!((amount + bump).to_string());
            lines.push(l);
        }
        lines
    };
    json!({"pricing_version":3,"input_token_limit":100,"output_token_limit":10,"price_lines":build(0),"batch_price_lines":build(1),"max_units":max_units})
}
/// Observes every view of the route's latest price: the deployment detail, and
/// the first-route price of the platform and workspace model comparisons.
async fn price_views<'a>(
    j: &mut Journey<'a>,
    admin: &'a Browser,
    owner: &'a Browser,
    team: Uuid,
    deployment: Uuid,
    models: (Uuid, Uuid),
) {
    let saved = j.actor;
    j.actor = admin;
    j.request(
        "GET",
        &format!("/api/v1/platform/deployments/{deployment}"),
        None,
    )
    .await;
    j.request(
        "GET",
        &format!(
            "/api/v1/platform/models/compare?ids={},{}",
            models.0, models.1
        ),
        None,
    )
    .await;
    j.actor = owner;
    j.request(
        "GET",
        &format!(
            "/api/v1/workspaces/{team}/models/compare?ids={},{}",
            models.0, models.1
        ),
        None,
    )
    .await;
    j.actor = saved;
}
/// The handler's verdict by status, except that a 400 whose machine reason is
/// one of `domain` refuses schema-valid input for a reason the schema cannot
/// express (live policy parents, stored state, server configuration): the
/// request got past input validation. Status alone cannot tell the two apart.
fn judged(captured: openapi::responses::Captured, domain: &[&str]) -> Probe {
    let domain_refusal = captured.status == StatusCode::BAD_REQUEST
        && serde_json::from_slice::<Value>(&captured.bytes)
            .ok()
            .and_then(|v| v["error"]["reason"].as_str().map(|r| domain.contains(&r)))
            .unwrap_or(false);
    if domain_refusal {
        Probe::decided(captured, true)
    } else {
        Probe::from(captured)
    }
}
/// A predictor of state-dependent domain refusals and the machine reasons that
/// are domain refusals (see [`judged_when`]).
type DomainRules<'a> = (&'a dyn Fn(&Value) -> bool, &'a [&'a str]);
/// The verdict a probe pins, written out literally: `(schema accepts, handler accepts)`.
/// A probe never derives its expectation from what the handler answered, so a probe
/// that stops discriminating (or a handler that changes) fails instead of agreeing.
type Expected = (bool, bool);
const BOTH_ACCEPT: Expected = (true, true);
const BOTH_REFUSE: Expected = (false, false);
/// A divergence the contract documents because the rule is not expressible: the
/// schema accepts, the handler refuses.
const SCHEMA_ONLY: Expected = (true, false);
/// A divergence the contract documents the other way round: the handler is the more
/// tolerant (non-canonical spellings, for example).
const HANDLER_ONLY: Expected = (false, true);
/// The request schema of an operation in the committed artifact.
fn request_schema(method: &str, template: &str) -> &'static Value {
    &openapi::responses::artifact()["paths"][template][method.to_ascii_lowercase()]["requestBody"]["content"]
        ["application/json"]["schema"]
}
/// Does the SCHEMA accept this body?
fn schema_accepts(method: &str, template: &str, body: &Value) -> bool {
    openapi::responses::validate_value(
        openapi::responses::artifact(),
        request_schema(method, template),
        body,
    )
    .is_ok()
}
/// Does the SCHEMA accept this raw value of a documented query parameter?
fn param_accepts(template: &str, name: &str, raw: &str) -> bool {
    let document = openapi::responses::artifact();
    let parameter = document["paths"][template]["get"]["parameters"]
        .as_array()
        .and_then(|all| all.iter().find(|p| p["name"] == name))
        .expect("documented query parameter");
    openapi::responses::validate_value(document, &parameter["schema"], &json!(raw)).is_ok()
}
/// The handler's verdict on an exchange, as `Probe` derives it: 400/422 refuse,
/// 2xx/3xx/404/409 accept; a 400 whose machine reason is in `domain` is a domain
/// refusal of valid input. Anything else is not an input verdict.
fn handler_accepts(captured: &openapi::responses::Captured, domain: &[&str]) -> bool {
    match captured.status.as_u16() {
        400 | 422 => serde_json::from_slice::<Value>(&captured.bytes)
            .ok()
            .and_then(|v| v["error"]["reason"].as_str().map(|r| domain.contains(&r)))
            .unwrap_or(false),
        200..=399 | 404 | 409 => true,
        other => panic!("unexpected status {other}: not an input verdict"),
    }
}
/// [`judged`] for requests whose 400 is ambiguous by stored STATE: the handler
/// refuses a body the schema accepts because of what the target already is (the
/// stored provider kind, the price's own cross-field rule). `rules.0` predicts
/// that from the sent body and the target; it is trusted ONLY for bodies the
/// schema accepts, so a schema-invalid body keeps the plain verdict (a 400 is
/// then agreement, and a missing refusal is a failure), and a schema-valid body
/// the predictor does not explain fails as a disagreement.
fn judged_when(
    captured: openapi::responses::Captured,
    schema_valid: bool,
    body: &Value,
    rules: DomainRules<'_>,
) -> Probe {
    // 400 from a handler check, 422 from a validating deserializer (PriceLines).
    if matches!(
        captured.status,
        StatusCode::BAD_REQUEST | StatusCode::UNPROCESSABLE_ENTITY
    ) && schema_valid
        && rules.0(body)
    {
        Probe::decided(captured, true)
    } else {
        judged(captured, rules.1)
    }
}
/// Whether the REAL price-line rules (`billing::v3::PriceLines`, which validates in
/// its deserializer) refuse a price body's `price_lines` or `batch_price_lines`, in
/// either wire form: the cross-line rules the contract documents but cannot state
/// (a not_applicable or unknown line next to another line of its meter, more than
/// 64 valued lines, duplicate (meter, variant, min_prompt_tokens) keys).
fn price_lines_refused(body: &Value) -> bool {
    let lists = if body.is_array() {
        [&body[6], &body[8]]
    } else {
        [&body["price_lines"], &body["batch_price_lines"]]
    };
    lists.into_iter().any(|lines| {
        lines.is_array()
            && serde_json::from_value::<crate::billing::v3::PriceLines>(lines.clone()).is_err()
    })
}
/// The price rules the contract documents but cannot state, decided for a price
/// body: a version 3 zero input ceiling (valid only when every input-family meter
/// is not applicable) and the cross-line rules of [`price_lines_refused`].
fn price_refused(b: &Value) -> bool {
    (b["pricing_version"] == 3 && b["input_token_limit"] == 0) || price_lines_refused(b)
}
/// Body boundary vectors with a state-dependent domain refusal (see [`judged_when`]).
async fn body_set_when(
    j: &mut Journey<'_>,
    who: &Browser,
    method: &str,
    template: &str,
    path: &str,
    valid: Value,
    rules: DomainRules<'_>,
) {
    let (client, root) = (j.client.clone(), j.base.clone());
    http_vectors::body_vectors_http(
        &mut j.observed,
        method,
        template,
        valid,
        async |body: Value| {
            let schema_valid = schema_accepts(method, template, &body);
            let captured = send(&client, &root, Some(who), method, path, Some(body.clone())).await;
            judged_when(captured, schema_valid, &body, rules)
        },
    )
    .await;
}
/// [`body_set_when`] with boundary `candidates` for patterns without derivable
/// length bounds (`(pointer, value)`, straddling within one edit).
#[allow(clippy::too_many_arguments)]
async fn body_set_when_candidates(
    j: &mut Journey<'_>,
    who: &Browser,
    method: &str,
    template: &str,
    path: &str,
    valid: Value,
    rules: DomainRules<'_>,
    candidates: &[(&str, Value)],
) {
    let (client, root) = (j.client.clone(), j.base.clone());
    http_vectors::body_vectors_extended(
        &mut j.observed,
        method,
        template,
        valid,
        &[],
        candidates,
        async |body: Value| {
            let schema_valid = schema_accepts(method, template, &body);
            let captured = send(&client, &root, Some(who), method, path, Some(body.clone())).await;
            judged_when(captured, schema_valid, &body, rules)
        },
    )
    .await;
}
/// A handler-derived body vector the artifact-derived ones cannot generate. Both
/// verdicts are pinned in `expected`: `BOTH_ACCEPT`/`BOTH_REFUSE` for agreement, and
/// `SCHEMA_ONLY`/`HANDLER_ONLY` for a divergence the contract cannot express (a
/// cross-item comparison, a byte count, arithmetic or server state), so the
/// description's claim stays tested.
#[allow(clippy::too_many_arguments)]
async fn extra_body(
    j: &Journey<'_>,
    who: &Browser,
    method: &str,
    template: &str,
    path: &str,
    label: &str,
    body: Value,
    expected: Expected,
) {
    let schema = schema_accepts(method, template, &body);
    let captured = send(&j.client, &j.base, Some(who), method, path, Some(body)).await;
    assert!(
        matches!(captured.status.as_u16(), 200..=399 | 400 | 404 | 409 | 422),
        "{method} {template}: {label}: status {} is not an input verdict",
        captured.status
    );
    let handler = handler_accepts(&captured, &[]);
    // Collected, not asserted one by one: a run reports every pinned verdict that
    // moved (labels and verdicts only, never a value), checked at the end.
    if (schema, handler) != expected {
        PROBE_FAILURES.with(|failures| {
            failures.borrow_mut().push(format!(
                "{method} {template}: {label}: schema accepts {schema}, handler accepts {handler}; pinned {expected:?}"
            ));
        });
    }
}
thread_local! {
    /// Pinned-probe mismatches of the running fixture (one test thread).
    static PROBE_FAILURES: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
}
fn enc(raw: &str) -> String {
    raw.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}
/// [`extra_body`] for one query parameter of a GET (`rest` carries the other,
/// valid, parameters; `raw` is the parameter's decoded value).
#[allow(clippy::too_many_arguments)]
async fn extra_query(
    j: &Journey<'_>,
    who: &Browser,
    template: &str,
    path: &str,
    rest: &str,
    param: &str,
    raw: &str,
    label: &str,
    expected: Expected,
) {
    let schema = param_accepts(template, param, raw);
    let target = format!(
        "{path}?{rest}{}{param}={}",
        if rest.is_empty() { "" } else { "&" },
        enc(raw)
    );
    let captured = send(&j.client, &j.base, Some(who), "GET", &target, None).await;
    assert!(
        matches!(captured.status.as_u16(), 200..=399 | 400 | 404 | 409 | 422),
        "GET {template}: {param} {label}: status {} is not an input verdict",
        captured.status
    );
    let handler = handler_accepts(&captured, &[]);
    assert_eq!(
        (schema, handler),
        expected,
        "GET {template}: {param} {label}: schema accepts {schema}, handler accepts {handler}"
    );
}
const THRESHOLD_MISSING: &str = "Set at least one server load threshold.";
const THRESHOLD_RANGE: &str = "A server load threshold is out of range.";
const METRICS_URL: &str =
    "The metrics URL must be /metrics on an approved local endpoint's origin.";
/// Did the handler's threshold rules accept the body? `RouteSettings::validate` checks the
/// thresholds BEFORE the metrics URL and the fixture gateway approves no local endpoint, so
/// a 400 that is the URL rule proves the thresholds passed, and a 400 that is a threshold
/// rule proves they did not: the handler's own message tells the two refusals apart (a
/// bare status cannot).
fn thresholds_accepted(captured: &openapi::responses::Captured) -> bool {
    if captured.status.is_success() {
        return true;
    }
    assert_eq!(
        captured.status,
        StatusCode::BAD_REQUEST,
        "scheduling vector: status is not a scheduling verdict"
    );
    let refusal: Value = serde_json::from_slice(&captured.bytes).unwrap();
    match refusal["error"]["message"].as_str() {
        Some(THRESHOLD_MISSING | THRESHOLD_RANGE) => false,
        Some(METRICS_URL) => true,
        _ => panic!("scheduling vector: the 400 is neither a threshold rule nor the URL rule"),
    }
}
/// The same rules run directly with the metrics URL APPROVED (as the real setup flow has
/// it), so only the thresholds remain: `None` when the body is not a settings document.
fn thresholds_with_approved_url(body: &Value) -> Option<bool> {
    let approvals =
        crate::providers::local::endpoints::ApprovedEndpoints::for_test("http://127.0.0.1:8000/v1");
    let mut body = body.clone();
    let url = json!("http://127.0.0.1:8000/metrics");
    match body.get_mut("metrics") {
        Some(Value::Object(gate)) => {
            gate.insert("url".into(), url);
        }
        Some(Value::Array(gate)) if !gate.is_empty() => gate[0] = url,
        _ => return None,
    }
    let settings: crate::jobs::schedule::RouteSettings = serde_json::from_value(body).ok()?;
    match settings.validate("vllm", &approvals) {
        Ok(()) => Some(true),
        Err(THRESHOLD_MISSING | THRESHOLD_RANGE) => Some(false),
        Err(_) => panic!("scheduling vector: a rule other than the thresholds refused the body"),
    }
}
/// A scheduling body vector pinned as `(schema accepts, thresholds accepted)`: the wire
/// verdict is read from the handler's message (see [`thresholds_accepted`]) and, where the
/// body is a settings document, cross-checked against the rules run with the URL approved.
#[allow(clippy::too_many_arguments)]
async fn extra_schedule(
    j: &Journey<'_>,
    who: &Browser,
    template: &str,
    path: &str,
    label: &str,
    body: Value,
    expected: Expected,
) {
    let schema = schema_accepts("PUT", template, &body);
    let direct = thresholds_with_approved_url(&body);
    let captured = send(&j.client, &j.base, Some(who), "PUT", path, Some(body)).await;
    let handler = thresholds_accepted(&captured);
    assert_eq!(
        (schema, handler),
        expected,
        "PUT {template}: {label}: schema accepts {schema}, thresholds accepted {handler}"
    );
    if let Some(direct) = direct {
        assert_eq!(
            direct, handler,
            "PUT {template}: {label}: the approved-URL rules say {direct}, the wire says {handler}"
        );
    }
}
/// Query boundary vectors for one operation: every documented parameter in the base.
async fn query_set(
    j: &mut Journey<'_>,
    who: &Browser,
    template: &str,
    path: &str,
    base: &[(&str, &str)],
) {
    let (client, root) = (j.client.clone(), j.base.clone());
    http_vectors::query_vectors_fixed(
        &mut j.observed,
        "GET",
        template,
        base,
        async |query: String| {
            send(
                &client,
                &root,
                Some(who),
                "GET",
                &format!("{path}?{query}"),
                None,
            )
            .await
        },
    )
    .await;
}
/// [`query_set`] with boundary `candidates` for a parameter whose pattern has no
/// derivable length bounds (the schema computes each candidate's verdict).
async fn query_set_with(
    j: &mut Journey<'_>,
    who: &Browser,
    template: &str,
    path: &str,
    base: &[(&str, &str)],
    candidates: &[(&str, &str)],
) {
    let (client, root) = (j.client.clone(), j.base.clone());
    let fixed: Vec<(String, String)> = base
        .iter()
        .map(|(n, v)| ((*n).to_owned(), (*v).to_owned()))
        .collect();
    http_vectors::query_vectors_extended(
        &mut j.observed,
        "GET",
        template,
        &[],
        candidates,
        async || ((), fixed.clone()),
        async |_: &(), query: String| {
            Probe::from(
                send(
                    &client,
                    &root,
                    Some(who),
                    "GET",
                    &format!("{path}?{query}"),
                    None,
                )
                .await,
            )
        },
    )
    .await;
}
/// Body boundary vectors for one operation with a fixed target.
async fn body_set(
    j: &mut Journey<'_>,
    who: &Browser,
    method: &str,
    template: &str,
    path: &str,
    valid: Value,
    domain: &[&str],
) {
    let (client, root) = (j.client.clone(), j.base.clone());
    http_vectors::body_vectors_http(
        &mut j.observed,
        method,
        template,
        valid,
        async |body: Value| {
            judged(
                send(&client, &root, Some(who), method, path, Some(body)).await,
                domain,
            )
        },
    )
    .await;
}
/// A model and an enabled route created through the API exactly as an operator
/// would (the connection profile serves the protocols: `route_unsupported_capability`
/// otherwise). The actor must be the platform admin.
async fn api_route(
    j: &mut Journey<'_>,
    provider: Uuid,
    public_name: &str,
    protocols: &[&str],
    upstream: &str,
) -> (Uuid, Uuid) {
    let model = id(&j
        .request(
            "POST",
            "/api/v1/platform/models",
            Some(json!({"public_name":public_name,"display_name":public_name,"enabled":true,"supported_protocols":protocols})),
        )
        .await);
    let deployment = id(&j
        .request(
            "POST",
            "/api/v1/platform/deployments",
            Some(json!({"model_id":model,"provider_connection_id":provider,"upstream_model":upstream,"enabled":true})),
        )
        .await);
    (model, deployment)
}
/// A telemetry attempt exactly as the engine records it: the execution row with
/// its timings and the governance reservation (settled with an actual cost, or
/// unknown with only a retained hold). The maintaining triggers fill the totals.
#[allow(clippy::too_many_arguments)]
async fn seed_attempt(
    pool: &PgPool,
    ws: Uuid,
    key: Uuid,
    deployment: Uuid,
    model: &str,
    state: &str,
    actual: Option<i64>,
    held: Option<i64>,
    reported: Option<(i64, i64, i64)>,
) -> Uuid {
    let id = Uuid::new_v4();
    // Provider-reported usage and cost (OpenRouter's usage accounting) when given.
    let tokens = reported
        .map(|r| (r.0, r.1))
        .or(actual.map(|_| (10_i64, 5_i64)));
    sqlx::query("INSERT INTO inference_executions(id,workspace_id,api_key_id,deployment_id,public_model,provider,streamed,state,started_at,completed_at,elapsed_ms,root_request_id,attempt_number,input_tokens,output_tokens,finish_reason,time_to_first_token_ms,generation_ms,provider_cost_microusd) VALUES($1,$2,$3,$4,$5,'openrouter',true,$6,now()-interval '1 second',now()-interval '1 second'+interval '600 milliseconds',600,$1,1,$7,$8,'stop',100,600,$9)")
        .bind(id).bind(ws).bind(key).bind(deployment).bind(model).bind(state)
        .bind(tokens.map(|t| t.0)).bind(tokens.map(|t| t.1)).bind(reported.map(|r| r.2))
        .execute(pool).await.unwrap();
    let reservation = if actual.is_some() {
        "settled"
    } else {
        "unknown"
    };
    sqlx::query("INSERT INTO governance_reservations(execution_id,workspace_id,api_key_id,deployment_id,admitted_at,minute_start,month_start,lease_expires_at,state,actual_microusd,held_microusd,unbounded_cost,input_tokens,output_tokens) SELECT $1,$2,$3,$4,e.started_at,date_trunc('minute',e.started_at,'UTC'),date_trunc('month',e.started_at,'UTC'),e.started_at,$5,$6,$7,$8,$9,$10 FROM inference_executions e WHERE e.id=$1")
        .bind(id).bind(ws).bind(key).bind(deployment).bind(reservation).bind(actual).bind(held)
        .bind(actual.is_none() && held.is_none())
        .bind(tokens.map(|t| t.0)).bind(tokens.map(|t| t.1))
        .execute(pool).await.unwrap();
    id
}
/// A route inserted as an earlier release accepted it (route validation did not
/// exist): used only for the `videos` route, which no connection profile serves
/// any more, so the API refuses to create it.
async fn legacy_route(
    pool: &PgPool,
    provider: Uuid,
    public_name: &str,
    protocols: &[&str],
    upstream: &str,
) -> (Uuid, Uuid) {
    let (model, deployment) = (Uuid::new_v4(), Uuid::new_v4());
    sqlx::query("INSERT INTO models(id,public_name,supported_protocols) VALUES($1,$2,$3)")
        .bind(model)
        .bind(public_name)
        .bind(
            protocols
                .iter()
                .map(|p| (*p).to_owned())
                .collect::<Vec<_>>(),
        )
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO deployments(id,model_id,provider_connection_id,upstream_model,enabled) VALUES($1,$2,$3,$4,true)")
        .bind(deployment)
        .bind(model)
        .bind(provider)
        .bind(upstream)
        .execute(pool)
        .await
        .unwrap();
    (model, deployment)
}
/// A policy body with every limit supplied (tighten-only: local <= platform).
#[allow(clippy::too_many_arguments)]
fn limits(
    rpm: i64,
    tpm: i64,
    concurrent: i64,
    jobs: i64,
    storage: i64,
    holder: i64,
    workspace: i64,
    budgets: Value,
) -> Value {
    json!({"requests_per_minute":rpm,"tokens_per_minute":tpm,"concurrent_requests":concurrent,"concurrent_jobs":jobs,"storage_bytes":storage,"key_issuances_per_holder":holder,"key_issuances_per_workspace":workspace,"budgets":budgets})
}
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn management_openapi_slice3_fixtures(pool: PgPool) {
    let mut f = fixture(&pool).await;
    f.s = f.s.with_secret_allowlists(
        crate::providers::secrets::EnvSecrets::from_lookup(["CONTRACT_REFERENCE".into()], |_| None),
        Default::default(),
    );
    let admin = browser(&pool, &f.admin, false).await;
    let owner = browser(&pool, &f.owner, false).await;
    let member = browser(&pool, &f.member, false).await;
    let auditor = browser(&pool, &f.auditor, false).await;
    let mock = crate::identity::tests::MockProvider::start(None).await;
    let identity = mock.state(pool.clone()).await;
    let engine = crate::inference::Engine::new(
        std::sync::Arc::new(f.s.clone()),
        Default::default(),
        Default::default(),
    )
    .unwrap();
    // The OpenRouter public catalog, one model per drafting branch. `contract/model`
    // lists endpoints (the endpoint-priced branch); the others have none, so the
    // catalog's top-provider price is used. Tiers and cache keys, missing
    // ceilings and prices, and each non-chat workload's units are all real
    // catalog shapes.
    let catalog_models = json!({"data":[
        {"id":"contract/model","context_length":8192,"top_provider":{"max_completion_tokens":1024},"pricing":{"prompt":"0.000000123","completion":"0.000000456"}},
        {"id":"contract/catalog-only","context_length":8192,"top_provider":{"max_completion_tokens":1024},"pricing":{"prompt":"0.000000123","completion":"0.000000456","input_cache_read":"0.00000001","input_cache_write":"0.000000125","input_cache_write_1h":"0.0000002","overrides":[{"min_prompt_tokens":4000,"prompt":"0.0000005","completion":"0.0000025","input_cache_read":"0.00000005","input_cache_write":"0.000000625","input_cache_write_1h":"0.000001"}]}},
        {"id":"contract/bare","pricing":{}},
        {"id":"contract/embed","context_length":8192,"pricing":{"prompt":"0.00000002"}},
        {"id":"contract/image","pricing":{}},
        {"id":"contract/image-many","pricing":{}},
        {"id":"contract/stt-minute","pricing":{"prompt":"0.0001","completion":"0.00001"}},
        {"id":"contract/stt-hour","pricing":{"prompt":"0.0000000025"}},
        {"id":"contract/tts-minute","pricing":{"prompt":"0.000015","completion":"0.0001"}},
        {"id":"contract/tts-hour","pricing":{"prompt":"0.000015","completion":"0.0000000025"}}
    ]});
    let model_endpoints = json!({"data":{"id":"contract/model","endpoints":[{"provider_name":"Contract Cheap","status":0,"pricing":{"prompt":"0.000000123","completion":"0.000000456"}},{"provider_name":"Contract Dear","status":0,"pricing":{"prompt":"0.000000246","completion":"0.000000912"}}]}});
    let image_endpoints = json!({"data":{"endpoints":[{"pricing":[{"billable":"output_image","unit":"image","cost_usd":0.041,"variant":"768"},{"billable":"output_image","unit":"image","cost_usd":0.06,"variant":"1024"}]}]}});
    // An image model priced per size with more sizes than a price may carry (64
    // valued lines): the draft's own lines fail PriceLines validation, so the
    // suggestion has no display lines and needs review.
    let many_images = json!({"data":{"endpoints":[{"pricing":(0..70).map(|n| json!({"billable":"output_image","unit":"image","cost_usd":0.04,"variant":format!("size-{n}")})).collect::<Vec<_>>()}]}});
    let catalog_app = axum::Router::new()
        .route(
            "/models",
            axum::routing::get(move || {
                let body = catalog_models.clone();
                async move { Json(body) }
            }),
        )
        .route(
            "/models/contract/model/endpoints",
            axum::routing::get(move || {
                let body = model_endpoints.clone();
                async move { Json(body) }
            }),
        )
        .route(
            "/images/models/contract/image-many/endpoints",
            axum::routing::get(move || {
                let body = many_images.clone();
                async move { Json(body) }
            }),
        )
        .route(
            "/images/models/contract/image/endpoints",
            axum::routing::get(move || {
                let body = image_endpoints.clone();
                async move { Json(body) }
            }),
        );
    let (catalog_url, catalog_task) = governance::suggestion::tests::mock(catalog_app).await;
    let _catalog_server = Server(catalog_task);
    let catalog = std::sync::Arc::new(governance::suggestion::OpenRouterCatalog::for_test(
        catalog_url,
        std::time::Duration::from_secs(1),
    ));
    let app = crate::http::router_with_identity(f.s.clone(), None, engine, identity)
        .layer(Extension(catalog));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let _server = Server(tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap()
    }));
    let mut j = Journey {
        client: reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap(),
        base,
        actor: &admin,
        observed: Default::default(),
    };
    let ws_policy = format!("/api/v1/workspaces/{}/policy", f.team);
    // Policy layers, state A: nothing configured anywhere (every limit absent).
    for who in [&owner, &member] {
        j.actor = who;
        j.request("GET", &ws_policy, None).await;
    }
    j.actor = &admin;
    let cat = id(&j
        .request(
            "POST",
            "/api/v1/platform/catalogs",
            Some(json!({"name":"Contract catalog","description":"Live defaults"})),
        )
        .await);
    let provider = id(&j.request("POST", "/api/v1/platform/providers", Some(json!({"name":"Contract reference","provider":"openrouter","credential_ref":"env:CONTRACT_REFERENCE","enabled":true}))).await);
    // A connection nothing uses, so its deletion is observed without touching the route fixtures.
    let disposable = id(&j.request("POST", "/api/v1/platform/providers", Some(json!({"name":"Contract disposable","provider":"openrouter","credential_ref":"env:CONTRACT_REFERENCE","enabled":false}))).await);
    // Identity-bearing provider kinds: Bedrock default and role identities (non-null
    // `aws_auth`), created through the real route. A named AWS profile needs a
    // server allowlist and a credential-less (`none`) local endpoint needs a
    // process-level GATEWAY_LOCAL_UPSTREAMS approval; the test process sets
    // neither, so those connections are inserted directly (the same way the
    // batch-scheduling tests seed local routes).
    let bedrock_default = id(&j.request("POST", "/api/v1/platform/providers", Some(json!({"name":"Contract Bedrock","provider":"bedrock","credential_ref":"aws:default","endpoint":null,"region":"us-east-1","enabled":false}))).await);
    j.request("POST", "/api/v1/platform/providers", Some(json!({"name":"Contract Bedrock role","provider":"bedrock","credential_ref":"aws:role:arn:aws:iam::123456789012:role/gateway/bedrock-invoke","aws_external_id":"tenant-ext-42","aws_session_name":"gateway.contract","endpoint":null,"region":"us-east-1","enabled":false}))).await;
    let local_provider = Uuid::new_v4();
    let vllm_provider = Uuid::new_v4();
    for (connection, name, kind, reference, endpoint, region) in [
        (
            Uuid::new_v4(),
            "Contract profile",
            "bedrock",
            "aws:profile:contract",
            None,
            Some("us-east-1"),
        ),
        (
            local_provider,
            "Contract local",
            "ollama",
            "none",
            Some("http://10.0.0.5:11434/v1"),
            None,
        ),
        (
            vllm_provider,
            "Contract vLLM",
            "vllm",
            "none",
            Some("http://10.0.0.5:8000/v1"),
            None,
        ),
    ] {
        sqlx::query("INSERT INTO provider_connections(id,name,provider,credential_ref,endpoint,region,enabled) VALUES($6,$1,$2,$3,$4,$5,false)")
            .bind(name)
            .bind(kind)
            .bind(reference)
            .bind(endpoint)
            .bind(region)
            .bind(connection)
            .execute(&pool)
            .await
            .unwrap();
    }
    let model = id(&j.request("POST", "/api/v1/platform/models", Some(json!({"public_name":"contract/model","display_name":"Contract model","enabled":true}))).await);
    let unpriced = id(&j.request("POST", "/api/v1/platform/models", Some(json!({"public_name":"contract/unpriced","display_name":"Contract unpriced","enabled":true}))).await);
    let deployment = id(&j.request("POST", "/api/v1/platform/deployments", Some(json!({"model_id":model,"provider_connection_id":provider,"upstream_model":"contract/model","enabled":true}))).await);
    // Routing is configured first: `deployment` is the model's first route.
    j.request("PUT", &format!("/api/v1/platform/deployments/{deployment}/routing"), Some(json!({"priority":-1,"weight":1,"residency":null,"failure_threshold":3,"cooldown_seconds":30}))).await;
    j.request("PUT", &format!("/api/v1/platform/models/{model}/routing"), Some(json!({"strategy":"priority","max_attempts":2,"allow_ambiguous_failover":false,"required_residency":null}))).await;
    j.request(
        "GET",
        &format!("/api/v1/platform/models/{model}/routing"),
        None,
    )
    .await;
    // Second model through the one-call setup, in the catalog (workspace-visible).
    let setup = j.request("POST", "/api/v1/platform/model-setup", Some(json!({"model":{"public_name":"contract/second","display_name":"Second model","enabled":true},"route":{"provider_connection_id":provider,"upstream_model":"contract/model","enabled":true},"price":v3_price(),"catalog_ids":[cat]}))).await;
    let second = Uuid::parse_str(setup["model_id"].as_str().unwrap()).unwrap();
    // A setup without a price publishes none (price_id null).
    let unpriced_setup = j.request("POST", "/api/v1/platform/model-setup", Some(json!({"model":{"public_name":"contract/third","display_name":"Third model","enabled":true},"route":{"provider_connection_id":provider,"upstream_model":"contract/third","enabled":true}}))).await;
    assert!(unpriced_setup["price_id"].is_null());
    // Chat without any catalog data, created through the API; and one legacy
    // video route (no profile serves videos any more, so it can only be seeded)
    // that also anchors the telemetry.
    let (_, bare) = api_route(
        &mut j,
        provider,
        "contract/bare-model",
        &["chat_completions"],
        "contract/bare",
    )
    .await;
    let (legacy_model, anchor) = legacy_route(
        &pool,
        provider,
        "contract/legacy-video",
        &["videos"],
        "contract/legacy",
    )
    .await;
    // A route on a regional (Bedrock) connection: the deployment shows its region.
    let (_, bedrock_route) = api_route(
        &mut j,
        bedrock_default,
        "contract/bedrock-model",
        &["chat_completions"],
        "anthropic.claude-3-haiku-20240307-v1:0",
    )
    .await;
    j.request(
        "GET",
        &format!("/api/v1/platform/deployments/{bedrock_route}"),
        None,
    )
    .await;
    let mut drafted = vec![bare];
    for (name, protocols, upstream) in [
        (
            "contract/embed-model",
            &["embeddings"][..],
            "contract/embed",
        ),
        ("contract/image-model", &["images"][..], "contract/image"),
        (
            "contract/image-many-model",
            &["images"][..],
            "contract/image-many",
        ),
        (
            "contract/stt-minute-model",
            &["audio_transcriptions"][..],
            "contract/stt-minute",
        ),
        (
            "contract/stt-hour-model",
            &["audio_transcriptions"][..],
            "contract/stt-hour",
        ),
        (
            "contract/tts-minute-model",
            &["audio_speech"][..],
            "contract/tts-minute",
        ),
        (
            "contract/tts-hour-model",
            &["audio_speech"][..],
            "contract/tts-hour",
        ),
    ] {
        drafted.push(
            api_route(&mut j, provider, name, protocols, upstream)
                .await
                .1,
        );
    }
    j.request(
        "PATCH",
        &format!("/api/v1/platform/catalogs/{cat}"),
        Some(json!({"name":"Contract renamed","description":"Renamed catalog"})),
    )
    .await;
    j.request(
        "PATCH",
        &format!("/api/v1/platform/providers/{provider}"),
        Some(json!({"enabled":true})),
    )
    .await;
    j.request(
        "PATCH",
        &format!("/api/v1/platform/models/{model}"),
        Some(json!({"description":"Description"})),
    )
    .await;
    j.request(
        "PUT",
        &format!("/api/v1/platform/catalogs/{cat}/models"),
        Some(json!({"model_ids":[model,second,unpriced]})),
    )
    .await;
    j.request(
        "PUT",
        &format!("/api/v1/platform/models/{model}/catalogs"),
        Some(json!({"catalog_ids":[cat]})),
    )
    .await;
    j.request(
        "PUT",
        "/api/v1/platform/catalog-defaults",
        Some(json!({"personal":[cat],"team":[cat],"project":[cat]})),
    )
    .await;
    // Policy layers, state A2: the platform default for teams is cleared (no
    // type default limits anywhere), as are the readiness facts derived from it.
    let r = send(
        &j.client,
        &j.base,
        Some(&admin),
        "PUT",
        "/api/v1/platform/workspace-types/team/policy",
        Some(json!({"requests_per_minute":null,"tokens_per_minute":null,"concurrent_requests":null,"concurrent_jobs":null,"storage_bytes":null,"key_issuances_per_holder":null,"key_issuances_per_workspace":null,"budgets":[]})),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK);
    for who in [&owner, &member] {
        j.actor = who;
        j.request("GET", &ws_policy, None).await;
    }
    j.actor = &admin;
    j.request("GET", "/api/v1/platform/models", None).await;
    j.request("GET", &format!("/api/v1/platform/models/{model}"), None)
        .await;
    // Policy layers, state B: the platform default for teams is set.
    let r = send(
        &j.client,
        &j.base,
        Some(&admin),
        "PUT",
        "/api/v1/platform/workspace-types/team/policy",
        Some(limits(
            100,
            100_000,
            10,
            5,
            1_000_000,
            50,
            500,
            json!([{"period":"month","amount_microusd":"9007199254740993"}]),
        )),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK);
    for who in [&owner, &member] {
        j.actor = who;
        j.request("GET", &ws_policy, None).await;
    }
    j.actor = &admin;
    // The price history of the route, every immutable version shape, with each
    // version's views (route detail, platform and workspace comparison) while it
    // is the latest: legacy and v2 (all cache-rate states), then v3 with every
    // meter and unit, then v3 with mostly not-applicable meters.
    let models = (model, second);
    price_views(&mut j, &admin, &owner, f.team, deployment, models).await;
    j.request(
        "POST",
        &format!("/api/v1/platform/deployments/{deployment}/prices"),
        Some(legacy_price()),
    )
    .await;
    price_views(&mut j, &admin, &owner, f.team, deployment, models).await;
    let cache = |read: Value, write: Value, write_5m: Value, write_1h: Value| json!({"pricing_version":2,"input_microusd_per_million":"1","output_microusd_per_million":"2","input_token_limit":100,"output_token_limit":10,"cache_pricing":{"read":read,"write":write,"write_5m":write_5m,"write_1h":write_1h}});
    let priced = |amount: &str| json!({"status":"priced","microusd_per_million":amount});
    let unknown = || json!({"status":"unknown"});
    let na = || json!({"status":"not_applicable"});
    for body in [
        cache(priced("1"), unknown(), na(), priced("2")),
        cache(unknown(), na(), priced("3"), unknown()),
        cache(na(), priced("4"), unknown(), na()),
    ] {
        j.request(
            "POST",
            &format!("/api/v1/platform/deployments/{deployment}/prices"),
            Some(body),
        )
        .await;
        price_views(&mut j, &admin, &owner, f.team, deployment, models).await;
    }
    let rich = v3_with(
        &[
            line("input_tokens", 1_000_000, 123_000),
            line("output_tokens", 1_000_000, 456_000),
            line("cache_read_tokens", 1_000_000, 12_000),
            line("cache_write_tokens", 1_000_000, 150_000),
            line("cache_write_5m_tokens", 1_000_000, 160_000),
            line("cache_write_1h_tokens", 1_000_000, 240_000),
            line("output_images", 1, 40_000),
            line("input_characters", 1_000_000, 15),
            line("input_audio_seconds_ms", 1000, 5),
            line("output_audio_seconds_ms", 1000, 6),
            line("search_units", 1, 7),
            line("requests", 1, 1),
        ],
        &[
            tiered(line("input_tokens", 1_000_000, 150_000), 4000),
            varied(line("output_images", 1, 50_000), "1024"),
            tiered(line("input_audio_seconds_ms", 60_000, 300), 10),
            tiered(line("input_audio_seconds_ms", 3_600_000, 18_000), 20),
            tiered(line("output_audio_seconds_ms", 60_000, 360), 10),
            tiered(line("output_audio_seconds_ms", 3_600_000, 21_600), 20),
            line("input_audio_tokens", 1_000_000, 100),
            line("cache_read_audio_tokens", 1_000_000, 10),
            line("output_audio_tokens", 1_000_000, 200),
            varied(line("output_video_seconds_ms", 1000, 9), "720p"),
            varied(line("output_video_seconds_ms", 60_000, 500), "1080p"),
            varied(line("output_video_seconds_ms", 3_600_000, 30_000), "4k"),
        ],
        json!({"requests":"1","output_images":"4","input_characters":"10000","input_audio_seconds_ms":"60000","output_audio_seconds_ms":"60000","search_units":"2","output_video_seconds_ms":"10000"}),
    );
    j.request(
        "POST",
        &format!("/api/v1/platform/deployments/{deployment}/prices"),
        Some(rich),
    )
    .await;
    price_views(&mut j, &admin, &owner, f.team, deployment, models).await;
    let sparse = v3_with(
        &[
            line("input_tokens", 1_000_000, 123),
            line("output_tokens", 1_000_000, 456),
            line("requests", 1, 1),
        ],
        &[],
        json!({"requests":"1"}),
    );
    j.request(
        "POST",
        &format!("/api/v1/platform/deployments/{deployment}/prices"),
        Some(sparse),
    )
    .await;
    price_views(&mut j, &admin, &owner, f.team, deployment, models).await;
    j.request(
        "POST",
        &format!("/api/v1/platform/deployments/{deployment}/prices"),
        Some(v3_price()),
    )
    .await;
    let mut mapped_price = v3_price();
    mapped_price["price_lines"][0]["meter"] = json!({"input_tokens":null});
    j.request(
        "POST",
        &format!("/api/v1/platform/deployments/{deployment}/prices"),
        Some(mapped_price),
    )
    .await;
    price_views(&mut j, &admin, &owner, f.team, deployment, models).await;
    // Route and model routing with a residency, then route health as the engine records it.
    j.request("PUT", &format!("/api/v1/platform/models/{model}/routing"), Some(json!({"strategy":"weighted","max_attempts":3,"allow_ambiguous_failover":true,"required_residency":"eu"}))).await;
    j.request(
        "GET",
        &format!("/api/v1/platform/models/{model}/routing"),
        None,
    )
    .await;
    j.request(
        "GET",
        &format!("/api/v1/platform/deployments/{deployment}/routing"),
        None,
    )
    .await;
    j.request("PUT", &format!("/api/v1/platform/deployments/{deployment}/routing"), Some(json!({"priority":-1,"weight":5,"residency":"eu","failure_threshold":3,"cooldown_seconds":30}))).await;
    j.request(
        "GET",
        &format!("/api/v1/platform/deployments/{deployment}/routing"),
        None,
    )
    .await;
    crate::routing::record_result(&f.s, deployment, None)
        .await
        .unwrap();
    j.request(
        "GET",
        &format!("/api/v1/platform/deployments/{deployment}/routing"),
        None,
    )
    .await;
    for _ in 0..3 {
        crate::routing::record_result(
            &f.s,
            deployment,
            Some(crate::inference::error::InferenceError::Timeout),
        )
        .await
        .unwrap();
    }
    j.request(
        "GET",
        &format!("/api/v1/platform/deployments/{deployment}/routing"),
        None,
    )
    .await;
    j.request("PUT", &format!("/api/v1/platform/deployments/{deployment}/batch-scheduling"), Some(json!({"max_concurrency":3,"yield_live_threshold":5,"window":{"timezone":"UTC","days":["mon","fri"],"start":"23:00","end":"06:00"}}))).await;
    // A vLLM-compatible route: defaults, then a priority and window, then
    // server-load gates (written by the production writer: the metrics URL needs
    // a process-level endpoint approval) and the route's batch signals.
    let (_, vllm) = api_route(
        &mut j,
        vllm_provider,
        "contract/vllm-model",
        &["chat_completions"],
        "contract/vllm",
    )
    .await;
    let schedule = format!("/api/v1/platform/deployments/{vllm}/batch-scheduling");
    j.request("GET", &schedule, None).await;
    j.request("PUT", &schedule, Some(json!({"max_concurrency":4,"yield_live_threshold":2,"priority":10,"window":{"timezone":"America/New_York","days":["mon","tue","wed","thu","fri"],"start":"19:00","end":"07:00"}}))).await;
    j.request("GET", &schedule, None).await;
    let gate = |waiting: Option<i32>, running: Option<i32>, kv: Option<i16>| {
        crate::jobs::schedule::RouteSettings {
            max_concurrency: 4,
            yield_live_threshold: Some(2),
            metrics: Some(crate::jobs::schedule::MetricsGate {
                url: "http://10.0.0.5:8000/metrics".into(),
                max_waiting: waiting,
                max_running: running,
                max_kv_cache_percent: kv,
            }),
            priority: Some(10),
            window: None,
        }
    };
    for (settings, signal) in [
        (
            gate(Some(5), None, Some(90)),
            "INSERT INTO deployment_batch_signals(deployment_id,paused_reason,checked_at,live_in_flight,metrics_checked_at,metrics_ok,metrics_waiting,metrics_running,metrics_kv_cache_permille,metrics_error) VALUES($1,'server_busy',clock_timestamp(),4,clock_timestamp(),true,7,3,950,NULL)",
        ),
        (
            gate(None, Some(8), None),
            "UPDATE deployment_batch_signals SET paused_reason=NULL,checked_at=clock_timestamp(),metrics_checked_at=clock_timestamp(),metrics_ok=false,metrics_waiting=NULL,metrics_running=NULL,metrics_kv_cache_permille=NULL,metrics_error='unreachable' WHERE deployment_id=$1",
        ),
    ] {
        let mut tx = pool.begin().await.unwrap();
        crate::jobs::schedule::save_settings(&mut tx, vllm, &settings, f.admin.user_id)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        sqlx::query(signal).bind(vllm).execute(&pool).await.unwrap();
        j.request("GET", &schedule, None).await;
    }
    sqlx::query("UPDATE deployment_batch_signals SET paused_reason='metrics_unavailable',checked_at=clock_timestamp() WHERE deployment_id=$1").bind(vllm).execute(&pool).await.unwrap();
    j.request("GET", &schedule, None).await;
    // Populated shared preview, and catalog summary's owner-private count.
    direct(&pool, f.team, model).await;
    direct(&pool, f.personal, model).await;
    sqlx::query("INSERT INTO workspace_catalog_overrides(workspace_id) VALUES($1)")
        .bind(f.personal)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO workspace_catalog_overrides(workspace_id) VALUES($1)")
        .bind(f.team)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO workspace_catalog_override_items(workspace_id,catalog_id) VALUES($1,$2),($3,$2)").bind(f.personal).bind(cat).bind(f.team).execute(&pool).await.unwrap();
    let preview = j
        .request(
            "POST",
            &format!("/api/v1/platform/deployments/{deployment}/configuration/preview"),
            Some(json!({"enabled":false})),
        )
        .await;
    assert_eq!(preview["affected_workspaces"]["total"], 1);
    assert!(!preview.to_string().contains(&f.personal.to_string()));
    j.request(
        "PATCH",
        &format!("/api/v1/platform/deployments/{deployment}"),
        Some(json!({"enabled":false})),
    )
    .await;
    // Re-enabling a disabled route gains a serving route.
    let gained = j
        .request(
            "POST",
            &format!("/api/v1/platform/deployments/{deployment}/configuration/preview"),
            Some(json!({"enabled":true})),
        )
        .await;
    assert_eq!(gained["serving_routes_gained"].as_array().unwrap().len(), 1);
    j.request(
        "PATCH",
        &format!("/api/v1/platform/deployments/{deployment}"),
        Some(json!({"enabled":true})),
    )
    .await;
    // The same change made with an admin token (catalog:write) records the token.
    let created = send(
        &j.client,
        &j.base,
        Some(&admin),
        "POST",
        "/api/v1/platform/admin-tokens",
        Some(json!({"name":"Contract token","scopes":["catalog:write"],"expires_in_days":1})),
    )
    .await;
    assert_eq!(created.status, StatusCode::OK);
    let token = serde_json::from_slice::<Value>(&created.bytes).unwrap()["token"]
        .as_str()
        .unwrap()
        .to_owned();
    let path = format!("/api/v1/platform/deployments/{deployment}");
    let response = j
        .client
        .patch(format!("{}{path}", j.base))
        .header("origin", ORIGIN)
        .bearer_auth(&token)
        .json(&json!({"enabled":false}))
        .send()
        .await
        .unwrap();
    let r = capture_http("PATCH", &path, response).await;
    assert_eq!(r.status, StatusCode::OK);
    j.observed.observe(&r);
    j.request("PATCH", &path, Some(json!({"enabled":true})))
        .await;
    // Earlier releases audited deployment changes without a snapshot: one with a
    // named actor, one without an actor (system), both values unavailable.
    let (legacy_user, legacy_system) = (Uuid::new_v4(), Uuid::new_v4());
    sqlx::query("INSERT INTO audit_events(id,actor_user_id,action,resource_type,resource_id,metadata,created_at) VALUES($1,$2,'configuration.updated','deployments',$3,'{}',now()-interval '1 day')").bind(legacy_user).bind(f.admin.user_id).bind(deployment).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO audit_events(id,action,resource_type,resource_id,metadata,created_at) VALUES($1,'deployment.created','deployment',$2,'{}',now()-interval '2 days')").bind(legacy_system).bind(deployment).execute(&pool).await.unwrap();
    // Telemetry and accounting of `contract/model` for the comparisons and policy windows.
    let (_, owner_key) = key(&f, &f.owner, f.team, Value::Null).await;
    let owner_key = id(&owner_key);
    seed_attempt(
        &pool,
        f.team,
        owner_key,
        anchor,
        "contract/model",
        "succeeded",
        Some(50),
        None,
        None,
    )
    .await;
    for path in [
        "/api/v1/platform/catalogs".into(),
        format!("/api/v1/platform/catalogs/{cat}"),
        format!("/api/v1/platform/catalogs/{cat}/models"),
        "/api/v1/platform/catalog-defaults".into(),
        "/api/v1/platform/models".into(),
        format!("/api/v1/platform/models/{model}"),
        format!("/api/v1/platform/models/{second}"),
        format!("/api/v1/platform/models/{model}/catalogs"),
        format!("/api/v1/platform/models/{model}/routing"),
        "/api/v1/platform/providers".into(),
        format!("/api/v1/platform/providers/{provider}"),
        format!("/api/v1/platform/providers/{bedrock_default}"),
        format!("/api/v1/platform/providers/{local_provider}"),
        "/api/v1/platform/deployments".into(),
        format!("/api/v1/platform/deployments/{deployment}"),
        format!("/api/v1/platform/deployments/{deployment}/prices"),
        format!("/api/v1/platform/deployments/{deployment}/routing"),
        format!("/api/v1/platform/deployments/{deployment}/batch-scheduling"),
        format!("/api/v1/platform/deployments/{deployment}/price-suggestion"),
        format!("/api/v1/platform/models/compare?ids={model},{second}"),
        format!("/api/v1/platform/models/compare?ids={model},{unpriced}"),
    ] {
        let value = j.request("GET", &path, None).await;
        assert!(!value.to_string().contains(&f.personal.to_string()));
    }
    // A route with no model price data nor a legacy-compatible protocol is unsupported.
    let detail = j
        .request(
            "GET",
            &format!("/api/v1/platform/models/{legacy_model}"),
            None,
        )
        .await;
    assert_eq!(
        detail["readiness"]["unsupported_profiles"],
        json!(["openrouter"])
    );
    // Optional-parameter coverage: every documented query parameter in a positive request.
    // The OpenRouter route's data policy is the configured one; the two picker
    // exclusions name workspaces/catalogs the model is not yet part of.
    let policy_now = crate::providers::openrouter::DataCollection::configured().as_str();
    let nested = j.request("GET", &format!("/api/v1/platform/models?limit=5&offset=0&q=contract&enabled=true&provider_connection_id={provider}&type=generation&max_input_price=9007199254740993&data_policy={policy_now}&include_deprecated=true&sort=name&not_in_catalog={}&not_direct_in={}", Uuid::new_v4(), Uuid::new_v4()), None).await;
    assert!(!nested.to_string().contains(&f.personal.to_string()));
    for sort in ["newest", "price"] {
        j.request("GET", &format!("/api/v1/platform/models?sort={sort}"), None)
            .await;
    }
    j.request(
        "GET",
        "/api/v1/platform/catalogs?limit=5&offset=0&q=contract",
        None,
    )
    .await;
    j.request(
        "GET",
        &format!("/api/v1/platform/catalogs/{cat}/models?limit=5&offset=0"),
        None,
    )
    .await;
    j.request("GET", &format!("/api/v1/platform/deployments?enabled=true&limit=5&offset=0&model_id={model}&provider_connection_id={provider}&q=contract"), None).await;
    j.request(
        "GET",
        "/api/v1/platform/providers?limit=10&offset=0&q=contract&enabled=false",
        None,
    )
    .await;
    j.request(
        "GET",
        &format!("/api/v1/platform/deployments/{deployment}/prices?limit=2&offset=1"),
        None,
    )
    .await;
    // The price suggestion has two real branches for tokens: priced from a concrete
    // endpoint, and from the catalog's top provider when no endpoint list exists;
    // and every other workload's units, unknown prices and missing ceilings.
    let catalog_only = id(&j.request("POST", "/api/v1/platform/deployments", Some(json!({"model_id":model,"provider_connection_id":provider,"upstream_model":"contract/catalog-only","enabled":true}))).await);
    let from_catalog = j
        .request(
            "GET",
            &format!("/api/v1/platform/deployments/{catalog_only}/price-suggestion"),
            None,
        )
        .await;
    assert_eq!(from_catalog["endpoint"]["selection"], "catalog");
    let from_endpoint = j
        .request(
            "GET",
            &format!("/api/v1/platform/deployments/{deployment}/price-suggestion"),
            None,
        )
        .await;
    assert_eq!(from_endpoint["endpoint"]["selection"], "cheapest");
    // A succeeded attempt whose provider-reported cost is the dearer endpoint's list
    // price for its usage (1000 in + 1000 out: 246 + 912 micro-USD) identifies the
    // endpoint that served the route: the draft follows the evidence.
    let (_, evidence_key) = key(&f, &f.owner, f.team, Value::Null).await;
    seed_attempt(
        &pool,
        f.team,
        id(&evidence_key),
        deployment,
        "contract/model",
        "succeeded",
        Some(1),
        None,
        Some((1000, 1000, 1158)),
    )
    .await;
    let from_evidence = j
        .request(
            "GET",
            &format!("/api/v1/platform/deployments/{deployment}/price-suggestion"),
            None,
        )
        .await;
    assert_eq!(from_evidence["endpoint"]["selection"], "evidence");
    assert_eq!(from_evidence["endpoint"]["matched_attempts"], 1);
    for route in &drafted {
        j.request(
            "GET",
            &format!("/api/v1/platform/deployments/{route}/price-suggestion"),
            None,
        )
        .await;
    }
    // PlatformRead is the declared boundary, but the handler needs platform_write.
    let r = send(
        &j.client,
        &j.base,
        Some(&auditor),
        "GET",
        &format!("/api/v1/platform/deployments/{deployment}/price-suggestion"),
        None,
    )
    .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    // An explicit endpoint null is an endpoint update: Bedrock clears it; any other kind is refused.
    j.request(
        "PATCH",
        &format!("/api/v1/platform/providers/{bedrock_default}"),
        Some(json!({"enabled":false,"endpoint":null})),
    )
    .await;
    let r = send(
        &j.client,
        &j.base,
        Some(&admin),
        "PATCH",
        &format!("/api/v1/platform/providers/{provider}"),
        Some(json!({"enabled":true,"endpoint":null})),
    )
    .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    // Configuration history: paged (cursor), every kind of entry in detail.
    let history = format!("/api/v1/platform/deployments/{deployment}/configuration/history");
    let first = j.request("GET", &format!("{history}?limit=2"), None).await;
    let cursor = first["next_cursor"].as_str().unwrap().to_owned();
    j.request("GET", &format!("{history}?limit=2&cursor={cursor}"), None)
        .await;
    let all = j
        .request("GET", &format!("{history}?limit=100"), None)
        .await;
    let entries = all["data"].as_array().unwrap();
    assert!(
        entries
            .iter()
            .any(|e| e["actor"]["admin_token_id"].is_string())
    );
    assert!(entries.iter().any(|e| e["actor"]["id"].is_null()));
    assert!(entries.iter().any(|e| e["values_available"] == false));
    for entry in entries {
        j.request("GET", &format!("{history}/{}", id(entry)), None)
            .await;
    }
    j.actor = &owner;
    j.request(
        "POST",
        &format!("/api/v1/workspaces/{}/models", f.team),
        Some(json!({"model_id":second})),
    )
    .await;
    // Policy layers, state C: the team's own (tighter) limits, then its accounting:
    // a settled request, and afterwards one whose cost is unknown.
    let policy = limits(
        10,
        1000,
        2,
        1,
        10_000,
        5,
        50,
        json!([{"period":"month","amount_microusd":"9007199254740993"},{"period":"lifetime","amount_microusd":"9007199254740993"}]),
    );
    j.request(
        "PUT",
        &format!("/api/v1/workspaces/{}/policy", f.team),
        Some(policy.clone()),
    )
    .await;
    for who in [&owner, &member] {
        j.actor = who;
        j.request("GET", &ws_policy, None).await;
    }
    seed_attempt(
        &pool,
        f.team,
        owner_key,
        anchor,
        "contract/model",
        "failed",
        None,
        Some(7),
        None,
    )
    .await;
    for who in [&owner, &member] {
        j.actor = who;
        j.request("GET", &ws_policy, None).await;
    }
    j.actor = &owner;
    for suffix in ["models", "available-models", "catalog", "policy"] {
        j.request(
            "GET",
            &format!("/api/v1/workspaces/{}/{suffix}", f.team),
            None,
        )
        .await;
    }
    for suffix in [
        "models?limit=5&offset=0&q=contract",
        "available-models?limit=5&offset=0&q=contract",
        "catalog?limit=5&offset=0&q=contract&type=generation&sort=price",
    ] {
        j.request(
            "GET",
            &format!("/api/v1/workspaces/{}/{suffix}", f.team),
            None,
        )
        .await;
    }
    j.request(
        "GET",
        &format!(
            "/api/v1/workspaces/{}/models/compare?ids={model},{second}",
            f.team
        ),
        None,
    )
    .await;
    // Live member redaction: limits visible, aggregate consumption hidden.
    j.actor = &member;
    let redacted = j
        .request(
            "GET",
            &format!("/api/v1/workspaces/{}/policy", f.team),
            None,
        )
        .await;
    assert_eq!(redacted["storage"]["usage_visible"], false);
    assert!(redacted["storage"]["used_bytes"].is_null());
    assert_eq!(redacted["budgets"][0]["headroom_status"], "restricted");
    assert!(redacted["budgets"][0]["used_microusd"].is_null());
    j.request(
        "GET",
        &format!(
            "/api/v1/workspaces/{}/models/compare?ids={model},{second}",
            f.team
        ),
        None,
    )
    .await;
    // Invalid accounting: a throwaway team whose day window totals are corrupted
    // exactly as governance/tests.rs does (the maintaining trigger never writes
    // such rows). The owner sees that window with every consumption fact unknown
    // (the neighbouring month stays known); a member sees both restricted.
    let broken = workspace(
        &pool,
        "Contract invalid accounting",
        "team",
        f.owner.user_id,
    )
    .await;
    super::member(&pool, broken, f.member.user_id, "member").await;
    let broken_policy = format!("/api/v1/workspaces/{broken}/policy");
    j.actor = &owner;
    j.request(
        "PUT",
        &broken_policy,
        Some(limits(
            10,
            1000,
            2,
            1,
            10_000,
            5,
            50,
            json!([{"period":"day","amount_microusd":"100"},{"period":"month","amount_microusd":"1000"}]),
        )),
    )
    .await;
    let (_, broken_key) = key(&f, &f.owner, broken, Value::Null).await;
    seed_attempt(
        &pool,
        broken,
        id(&broken_key),
        anchor,
        "contract/model",
        "succeeded",
        Some(70),
        None,
        None,
    )
    .await;
    let corrupted = sqlx::query("UPDATE budget_totals SET settled_microusd=-1,held_microusd=0 WHERE scope_kind='workspace' AND scope_id=$1 AND period='day'")
        .bind(broken)
        .execute(&pool)
        .await
        .unwrap()
        .rows_affected();
    assert_eq!(corrupted, 1);
    for (who, day, month) in [
        (&owner, "unknown", "known"),
        (&member, "restricted", "restricted"),
    ] {
        j.actor = who;
        let v = j.request("GET", &broken_policy, None).await;
        // The local layer's own day and month windows (the platform default's month window
        // comes first).
        let windows = v["budgets"].as_array().unwrap();
        let window = |period: &str| {
            windows
                .iter()
                .find(|w| w["layer"] == "local" && w["period"] == period)
                .unwrap()
        };
        assert_eq!(window("day")["headroom_status"], day);
        assert_eq!(window("month")["headroom_status"], month);
        if day == "unknown" {
            // Every consumption fact travels together: all null, none fabricated.
            for field in [
                "used_microusd",
                "settled_microusd",
                "pending_held_microusd",
                "unknown_held_microusd",
                "unknown_cost_requests",
                "remaining_microusd",
                "exhausted",
                "unresolved_usage",
            ] {
                assert!(window("day")[field].is_null());
            }
        }
    }
    j.actor = &owner;
    j.request(
        "DELETE",
        &format!("/api/v1/workspaces/{}/models/{second}", f.team),
        None,
    )
    .await;
    // Catalog retirement cannot revoke independent direct assignments or
    // resurrect a retired catalog selection when availability returns.
    j.request(
        "POST",
        &format!("/api/v1/workspaces/{}/models", f.team),
        Some(json!({"model_id":second})),
    )
    .await;
    j.actor = &admin;
    j.request(
        "PUT",
        &format!("/api/v1/platform/catalogs/{cat}/models"),
        Some(json!({"model_ids":[]})),
    )
    .await;
    j.actor = &owner;
    let selected = j
        .request(
            "GET",
            &format!("/api/v1/workspaces/{}/models", f.team),
            None,
        )
        .await;
    assert_eq!(selected["data"].as_array().unwrap().len(), 1);
    assert_eq!(selected["data"][0]["direct_granted"], true);
    assert_eq!(selected["data"][0]["available_from_catalog"], false);
    let r = send(
        &j.client,
        &j.base,
        Some(&owner),
        "POST",
        &format!("/api/v1/workspaces/{}/models", f.team),
        Some(json!({"model_id":second})),
    )
    .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    j.actor = &admin;
    j.request(
        "PUT",
        &format!("/api/v1/platform/catalogs/{cat}/models"),
        Some(json!({"model_ids":[model,second]})),
    )
    .await;
    j.actor = &owner;
    let selected = j
        .request(
            "GET",
            &format!("/api/v1/workspaces/{}/models", f.team),
            None,
        )
        .await;
    assert_eq!(selected["data"].as_array().unwrap().len(), 1);
    j.actor = &admin;
    // Existing refusal assertions remain independent of contract validation.
    for actor in [&admin, &auditor] {
        let r = send(
            &j.client,
            &j.base,
            Some(actor),
            "GET",
            &format!("/api/v1/workspaces/{}/catalog", f.personal),
            None,
        )
        .await;
        assert_eq!(r.status, StatusCode::NOT_FOUND);
    }
    let r = send(
        &j.client,
        &j.base,
        Some(&auditor),
        "PATCH",
        &format!("/api/v1/platform/models/{model}"),
        Some(json!({"enabled":false})),
    )
    .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    let r = send(
        &j.client,
        &j.base,
        None,
        "GET",
        "/api/v1/platform/models",
        None,
    )
    .await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    let (status, inference_key) = key(&f, &f.owner, f.team, Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    let response = j
        .client
        .get(format!("{}/api/v1/platform/models", j.base))
        .bearer_auth(inference_key["token"].as_str().unwrap())
        .send()
        .await
        .unwrap();
    let r = capture_http("GET", "/api/v1/platform/models", response).await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    for (method, path) in [
        ("POST", "/api/v1/platform/providers".to_owned()),
        ("PUT", format!("/api/v1/workspaces/{}/policy", f.team)),
    ] {
        let valid = if method == "POST" {
            json!({"name":"Bad","provider":"openrouter","credential_ref":"env:CONTRACT_REFERENCE","enabled":true})
        } else {
            policy.clone()
        };
        let mut bad = valid;
        bad["unexpected_configuration"] = json!(true);
        let r = send(
            &j.client,
            &j.base,
            Some(if method == "POST" { &admin } else { &owner }),
            method,
            &path,
            Some(bad),
        )
        .await;
        assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
    }
    // HTTP boundary vectors: every documented query parameter and JSON request
    // body, each compared with the real handler over loopback. Targets that a
    // request consumes (catalog, model, route, workspace policy) are dedicated
    // ones created here; state-replacing operations run last.
    let vcat = id(&j
        .request(
            "POST",
            "/api/v1/platform/catalogs",
            Some(json!({"name":"Vector catalog"})),
        )
        .await);
    j.request("GET", &format!("/api/v1/platform/catalogs/{vcat}"), None)
        .await;
    let vmodel = id(&j.request("POST", "/api/v1/platform/models", Some(json!({"public_name":"vector/target","display_name":"Vector target","enabled":true}))).await);
    let vprov = id(&j.request("POST", "/api/v1/platform/providers", Some(json!({"name":"Vector connection","provider":"openrouter","credential_ref":"env:CONTRACT_REFERENCE","enabled":true}))).await);
    let vdep = id(&j.request("POST", "/api/v1/platform/deployments", Some(json!({"model_id":vmodel,"provider_connection_id":vprov,"upstream_model":"vector/upstream","enabled":true}))).await);
    let (a, o) = (&admin, &owner);
    let (model_s, second_s, prov_s) = (model.to_string(), second.to_string(), provider.to_string());
    let policy_s = policy_now;
    let ids = format!("{model_s},{second_s}");
    query_set(
        &mut j,
        a,
        "/api/v1/platform/catalogs",
        "/api/v1/platform/catalogs",
        &[("limit", "5"), ("offset", "0"), ("q", "contract")],
    )
    .await;
    query_set(
        &mut j,
        a,
        "/api/v1/platform/catalogs/{id}/models",
        &format!("/api/v1/platform/catalogs/{cat}/models"),
        &[("limit", "5"), ("offset", "0")],
    )
    .await;
    query_set(
        &mut j,
        a,
        "/api/v1/platform/deployments",
        "/api/v1/platform/deployments",
        &[
            ("limit", "5"),
            ("offset", "0"),
            ("q", "contract"),
            ("enabled", "true"),
            ("model_id", &model_s),
            ("provider_connection_id", &prov_s),
        ],
    )
    .await;
    query_set(
        &mut j,
        a,
        "/api/v1/platform/deployments/{deployment}/prices",
        &format!("/api/v1/platform/deployments/{deployment}/prices"),
        &[("limit", "2"), ("offset", "0")],
    )
    .await;
    let page = j.request("GET", &format!("{history}?limit=1"), None).await;
    let cursor = page["next_cursor"]
        .as_str()
        .unwrap_or("0_00000000-0000-0000-0000-000000000000")
        .to_owned();
    // The cursor's leading zeros are unbounded, so its pattern has no derivable length
    // bounds: candidates straddle the upper instant bound (one digit apart).
    query_set_with(
        &mut j,
        a,
        "/api/v1/platform/deployments/{id}/configuration/history",
        &history,
        &[("limit", "1"), ("cursor", &cursor)],
        &[
            (
                "cursor",
                "8210266876799999999_00000000-0000-0000-0000-000000000000",
            ),
            (
                "cursor",
                "8210266876899999999_00000000-0000-0000-0000-000000000000",
            ),
        ],
    )
    .await;
    let (unp_s, nd_s) = (Uuid::new_v4().to_string(), Uuid::new_v4().to_string());
    query_set(
        &mut j,
        a,
        "/api/v1/platform/models",
        "/api/v1/platform/models",
        &[
            ("limit", "5"),
            ("offset", "0"),
            ("q", "contract"),
            ("enabled", "true"),
            ("provider_connection_id", &prov_s),
            ("type", "generation"),
            ("max_input_price", "9007199254740993"),
            ("data_policy", policy_s),
            ("include_deprecated", "true"),
            ("sort", "name"),
            ("not_in_catalog", &unp_s),
            ("not_direct_in", &nd_s),
        ],
    )
    .await;
    query_set(
        &mut j,
        a,
        "/api/v1/platform/models/compare",
        "/api/v1/platform/models/compare",
        &[("ids", &ids)],
    )
    .await;
    query_set(
        &mut j,
        a,
        "/api/v1/platform/providers",
        "/api/v1/platform/providers",
        &[
            ("limit", "10"),
            ("offset", "0"),
            ("q", "contract"),
            ("enabled", "true"),
        ],
    )
    .await;
    let team = f.team;
    query_set(
        &mut j,
        o,
        "/api/v1/workspaces/{ws}/available-models",
        &format!("/api/v1/workspaces/{team}/available-models"),
        &[("limit", "5"), ("offset", "0"), ("q", "contract")],
    )
    .await;
    query_set(
        &mut j,
        o,
        "/api/v1/workspaces/{ws}/catalog",
        &format!("/api/v1/workspaces/{team}/catalog"),
        &[
            ("limit", "5"),
            ("offset", "0"),
            ("q", "contract"),
            ("type", "generation"),
            ("sort", "name"),
        ],
    )
    .await;
    query_set(
        &mut j,
        o,
        "/api/v1/workspaces/{ws}/models",
        &format!("/api/v1/workspaces/{team}/models"),
        &[("limit", "5"), ("offset", "0"), ("q", "contract")],
    )
    .await;
    query_set(
        &mut j,
        o,
        "/api/v1/workspaces/{ws}/models/compare",
        &format!("/api/v1/workspaces/{team}/models/compare"),
        &[("ids", &ids)],
    )
    .await;
    let catalog_body = json!({"name":"Vector catalog","description":"A catalog"});
    body_set(
        &mut j,
        a,
        "POST",
        "/api/v1/platform/catalogs",
        "/api/v1/platform/catalogs",
        catalog_body.clone(),
        &[],
    )
    .await;
    body_set(
        &mut j,
        a,
        "POST",
        "/api/v1/platform/catalogs",
        "/api/v1/platform/catalogs",
        json!(["Vector catalog positional", "A catalog"]),
        &[],
    )
    .await;
    body_set(
        &mut j,
        a,
        "PATCH",
        "/api/v1/platform/catalogs/{id}",
        &format!("/api/v1/platform/catalogs/{vcat}"),
        catalog_body,
        &[],
    )
    .await;
    // The positional wire form of the same body (serde's struct-as-array arm).
    body_set(
        &mut j,
        a,
        "PATCH",
        "/api/v1/platform/catalogs/{id}",
        &format!("/api/v1/platform/catalogs/{vcat}"),
        json!(["Vector catalog", "A catalog"]),
        &[],
    )
    .await;
    body_set(
        &mut j,
        a,
        "PUT",
        "/api/v1/platform/catalogs/{id}/models",
        &format!("/api/v1/platform/catalogs/{vcat}/models"),
        json!({"model_ids":[vmodel]}),
        &[],
    )
    .await;
    body_set(
        &mut j,
        a,
        "PUT",
        "/api/v1/platform/models/{id}/catalogs",
        &format!("/api/v1/platform/models/{vmodel}/catalogs"),
        json!({"catalog_ids":[vcat]}),
        &[],
    )
    .await;
    body_set(&mut j, a, "POST", "/api/v1/platform/models", "/api/v1/platform/models", json!({"public_name":"vector/created","display_name":"Vector created","description":"A model","enabled":true,"supported_protocols":["chat_completions"]}), &[]).await;
    body_set(&mut j, a, "PATCH", "/api/v1/platform/models/{id}", &format!("/api/v1/platform/models/{vmodel}"), json!({"public_name":"vector/target","display_name":"Vector target","description":"A model","enabled":true,"supported_protocols":["chat_completions"]}), &["route_unsupported_capability"]).await;
    // The other request shapes of the model PATCH: null keeps (public_name,
    // enabled), the single-workload protocol arm, and the positional wire form.
    for body in [
        json!({"public_name":null,"enabled":null}),
        json!({"supported_protocols":["embeddings"]}),
        json!([
            "vector/target",
            "Vector target",
            "A model",
            true,
            ["chat_completions"]
        ]),
        json!([null, "Vector target", "A model", null, ["embeddings"]]),
    ] {
        body_set(
            &mut j,
            a,
            "PATCH",
            "/api/v1/platform/models/{id}",
            &format!("/api/v1/platform/models/{vmodel}"),
            body,
            &["route_unsupported_capability"],
        )
        .await;
    }
    // The env reference pattern has no length bound (credential_valid checks the
    // alphabet, then the allowlist): its boundary is the alphabet, straddled by an
    // allowlisted name and the same name plus one character the pattern refuses.
    {
        let (client, root) = (j.client.clone(), j.base.clone());
        http_vectors::body_vectors_extended(
            &mut j.observed,
            "POST",
            "/api/v1/platform/providers",
            json!({"name":"Vector provider","provider":"openrouter","credential_ref":"env:CONTRACT_REFERENCE","endpoint":null,"region":null,"enabled":false,"aws_external_id":null,"aws_session_name":null}),
            &[],
            &[
                ("/credential_ref", json!("env:CONTRACT_REFERENCE")),
                ("/credential_ref", json!("env:CONTRACT_REFERENCE-")),
            ],
            async |body: Value| {
                judged(
                    send(&client, &root, Some(a), "POST", "/api/v1/platform/providers", Some(body)).await,
                    &[],
                )
            },
        )
        .await;
    }
    // A Bedrock connection: region, role identity and the AWS options it alone takes.
    let bedrock_body = json!({"name":"Vector Bedrock","provider":"bedrock","credential_ref":"aws:role:arn:aws:iam::123456789012:role/gateway/vector","aws_external_id":"tenant-ext-42","aws_session_name":"gateway.vector","endpoint":null,"region":"us-east-1","enabled":false});
    body_set(
        &mut j,
        a,
        "POST",
        "/api/v1/platform/providers",
        "/api/v1/platform/providers",
        bedrock_body.clone(),
        &[],
    )
    .await;
    let providers = "/api/v1/platform/providers";
    for (label, patch, expected) in [
        (
            "bedrock without a region",
            json!({"region":null}),
            BOTH_REFUSE,
        ),
        (
            "bedrock with a malformed region",
            json!({"region":"us-east-1x"}),
            BOTH_REFUSE,
        ),
        (
            "bedrock with an uppercase region",
            json!({"region":"US-EAST-1"}),
            BOTH_REFUSE,
        ),
        (
            "role options on the default identity",
            json!({"credential_ref":"aws:default"}),
            BOTH_REFUSE,
        ),
        (
            "session name on the default identity",
            json!({"credential_ref":"aws:default","aws_external_id":null}),
            BOTH_REFUSE,
        ),
        (
            "a ';' option in the reference",
            json!({"credential_ref":"aws:role:arn:aws:iam::123456789012:role/r;external_id=abcd","aws_external_id":null,"aws_session_name":null}),
            BOTH_REFUSE,
        ),
        (
            "a one character external id",
            json!({"aws_external_id":"x"}),
            BOTH_REFUSE,
        ),
        (
            "an external id with a space",
            json!({"aws_external_id":"ab cd"}),
            BOTH_REFUSE,
        ),
        (
            "an external id of 1224 characters",
            json!({"aws_external_id":"e".repeat(1224)}),
            BOTH_ACCEPT,
        ),
        (
            "an external id of 1225 characters",
            json!({"aws_external_id":"e".repeat(1225)}),
            BOTH_REFUSE,
        ),
        (
            "a session name of 64 characters",
            json!({"aws_session_name":"s".repeat(64)}),
            BOTH_ACCEPT,
        ),
        (
            "a session name of 65 characters",
            json!({"aws_session_name":"s".repeat(65)}),
            BOTH_REFUSE,
        ),
        (
            "a role without an account",
            json!({"credential_ref":"aws:role:arn:aws:iam:::role/r","aws_external_id":null,"aws_session_name":null}),
            BOTH_REFUSE,
        ),
        // The decisive length pairs the generated vectors cannot build (padding
        // breaks a structured region or ARN): x-omg-unsendable-boundary points here.
        // valid_region: at most 32 characters.
        (
            "a 32-character region",
            json!({"region":format!("us-{}-1", "a".repeat(27))}),
            BOTH_ACCEPT,
        ),
        (
            "a 33-character region",
            json!({"region":format!("us-{}-1", "a".repeat(28))}),
            BOTH_REFUSE,
        ),
        // A 2057-character role reference (the schema's bound: aws:role: plus a
        // 2048-character ARN) is refused by valid_role_arn's 510-character path
        // limit, the documented divergence; one more character is refused by both.
        (
            "a 2057-character role reference",
            json!({"credential_ref":format!("aws:role:arn:aws:iam::123456789012:role/{}n", format!("{}/", "a".repeat(125)).repeat(16)),"aws_external_id":null,"aws_session_name":null}),
            SCHEMA_ONLY,
        ),
        (
            "a 2058-character role reference",
            json!({"credential_ref":format!("aws:role:arn:aws:iam::123456789012:role/{}nn", format!("{}/", "a".repeat(125)).repeat(16)),"aws_external_id":null,"aws_session_name":null}),
            BOTH_REFUSE,
        ),
    ] {
        let mut body = bedrock_body.clone();
        body["name"] = json!(format!("Vector Bedrock {}", Uuid::new_v4()));
        for (k, v) in patch.as_object().unwrap() {
            body[k] = v.clone();
        }
        extra_body(
            &j,
            a,
            "POST",
            "/api/v1/platform/providers",
            providers,
            label,
            body,
            expected,
        )
        .await;
    }
    // Options need a restated reference; the stored kind then decides the rest.
    let vbed = id(&j.request("POST", providers, Some(json!({"name":"Vector Bedrock target","provider":"bedrock","credential_ref":"aws:default","endpoint":null,"region":"us-east-1","enabled":false}))).await);
    let patch_template = "/api/v1/platform/providers/{id}";
    let patch_bed = format!("/api/v1/platform/providers/{vbed}");
    let patch_or = format!("/api/v1/platform/providers/{vprov}");
    // update_provider refuses AWS options unless the restated reference is an aws:role:
    // reference, for every stored kind (stored_reference), and the schema says so: each
    // refusal below is pinned on both targets. The controls without options are accepted,
    // so the refusals are the options', not the reference's.
    let role = "aws:role:arn:aws:iam::123456789012:role/gateway/vector";
    for (label, body) in [
        (
            "an external id without a reference",
            json!({"enabled":true,"aws_external_id":"tenant-ext-42"}),
        ),
        (
            "a session name without a reference",
            json!({"enabled":true,"aws_session_name":"gateway.vector"}),
        ),
        (
            "options with a null reference",
            json!({"enabled":true,"credential_ref":null,"aws_external_id":"tenant-ext-42"}),
        ),
        (
            "an external id beside the default identity",
            json!({"enabled":true,"credential_ref":"aws:default","aws_external_id":"ab"}),
        ),
        (
            "a session name beside the default identity",
            json!({"enabled":true,"credential_ref":"aws:default","aws_session_name":"ab"}),
        ),
        (
            "an external id beside an env reference",
            json!({"enabled":true,"credential_ref":"env:CONTRACT_REFERENCE","aws_external_id":"ab"}),
        ),
        (
            "an external id beside none",
            json!({"enabled":true,"credential_ref":"none","aws_external_id":"ab"}),
        ),
    ] {
        for path in [&patch_bed, &patch_or] {
            extra_body(
                &j,
                a,
                "PATCH",
                patch_template,
                path,
                label,
                body.clone(),
                BOTH_REFUSE,
            )
            .await;
        }
    }
    for (path, label, reference) in [
        (&patch_bed, "the default identity restated", "aws:default"),
        (
            &patch_or,
            "the env reference restated",
            "env:CONTRACT_REFERENCE",
        ),
    ] {
        extra_body(
            &j,
            a,
            "PATCH",
            patch_template,
            path,
            label,
            json!({"enabled":true,"credential_ref":reference}),
            BOTH_ACCEPT,
        )
        .await;
    }
    // The PATCH reference's 2057-character bound, built by hand (padding breaks an
    // ARN): x-omg-unsendable-boundary points here. On the Bedrock connection a
    // 2057-character role reference is refused by valid_role_arn's 510-character
    // path limit (documented divergence); one more character is refused by both.
    let long_role = |name: &str| {
        format!(
            "aws:role:arn:aws:iam::123456789012:role/{}{name}",
            format!("{}/", "a".repeat(125)).repeat(16)
        )
    };
    for (label, reference, expected) in [
        (
            "a 2057-character role reference",
            long_role("n"),
            SCHEMA_ONLY,
        ),
        (
            "a 2058-character role reference",
            long_role("nn"),
            BOTH_REFUSE,
        ),
    ] {
        extra_body(
            &j,
            a,
            "PATCH",
            patch_template,
            &patch_bed,
            label,
            json!({"enabled":true,"credential_ref":reference}),
            expected,
        )
        .await;
    }
    // A role reference with options is the schema's way in; a connection that is not
    // Bedrock still refuses it (the stored kind is not in the body).
    extra_body(
        &j,
        a,
        "PATCH",
        patch_template,
        &patch_or,
        "role options on a non-Bedrock connection",
        json!({"enabled":true,"credential_ref":role,"aws_external_id":"ab"}),
        SCHEMA_ONLY,
    )
    .await;
    // The positional form has none of those couplings (documented divergence).
    for path in [&patch_bed, &patch_or] {
        extra_body(
            &j,
            a,
            "PATCH",
            patch_template,
            path,
            "a positional update with options beside the default identity",
            json!([true, "aws:default", "ab", null]),
            SCHEMA_ONLY,
        )
        .await;
    }
    // A Bedrock positional array has no region requirement either (documented divergence).
    extra_body(
        &j,
        a,
        "POST",
        "/api/v1/platform/providers",
        providers,
        "a positional Bedrock connection without a region",
        json!([
            "Vector positional",
            "bedrock",
            "aws:default",
            null,
            null,
            false,
            null,
            null
        ]),
        SCHEMA_ONLY,
    )
    .await;
    // Local kinds take no region (validate_connection): checked on the wire, where a
    // refusal could also be the server's missing approval, and against the very function
    // with the endpoint approved, where it cannot.
    let local_body = |region: Value| json!({"name":"Vector local","provider":"ollama","credential_ref":"none","endpoint":"http://127.0.0.1:11434/v1","region":region,"enabled":false});
    let local_approvals = crate::providers::local::endpoints::ApprovedEndpoints::for_test(
        "http://127.0.0.1:11434/v1",
    );
    for (label, region, expected) in [
        (
            "a local connection without a region",
            json!(null),
            BOTH_ACCEPT,
        ),
        (
            "a local connection with an empty region",
            json!(""),
            BOTH_ACCEPT,
        ),
        (
            "a local connection with a region",
            json!("us-east-1"),
            BOTH_REFUSE,
        ),
        (
            "a local connection with a blank region",
            json!(" "),
            BOTH_REFUSE,
        ),
    ] {
        let body = local_body(region.clone());
        let schema = schema_accepts("POST", "/api/v1/platform/providers", &body);
        let handler = local_approvals
            .validate_connection("http://127.0.0.1:11434/v1", "none", region.as_str())
            .is_ok();
        assert_eq!(
            (schema, handler),
            expected,
            "POST /api/v1/platform/providers: {label}: schema accepts {schema}, validate_connection accepts {handler}"
        );
    }
    for (label, region, expected) in [
        (
            "a local connection without a region (contract: approval is server state)",
            json!(null),
            SCHEMA_ONLY,
        ),
        (
            "a local connection with a region",
            json!("us-east-1"),
            BOTH_REFUSE,
        ),
    ] {
        let mut body = local_body(region);
        body["endpoint"] = json!("http://10.0.0.5:11434/v1");
        extra_body(
            &j,
            a,
            "POST",
            "/api/v1/platform/providers",
            providers,
            label,
            body,
            expected,
        )
        .await;
    }
    // What the schema cannot see is decided by the STORED kind, read from the connection
    // row rather than assumed per call: an OpenRouter connection takes no AWS options, no
    // endpoint and only the allowlisted env reference; a Bedrock connection with no
    // allowlisted profile or endpoint takes the default or a role reference and no endpoint
    // (options beside a non-role reference are the schema's own rule, so not trusted here).
    let refused_by_state = |kind: &str, b: &Value| {
        // The positional form in field order (enabled, credential_ref,
        // aws_external_id, aws_session_name, endpoint): the same stored-kind rules.
        let object;
        let b = if let Some(items) = b.as_array() {
            let mut map = serde_json::Map::new();
            for (name, value) in [
                "enabled",
                "credential_ref",
                "aws_external_id",
                "aws_session_name",
                "endpoint",
            ]
            .into_iter()
            .zip(items)
            {
                map.insert(name.to_owned(), value.clone());
            }
            object = Value::Object(map);
            &object
        } else {
            b
        };
        let reference = b["credential_ref"].as_str();
        let aws_options = b["aws_external_id"].is_string() || b["aws_session_name"].is_string();
        if kind == "bedrock" {
            b["endpoint"].is_string()
                || reference.is_some_and(|c| !(c == "aws:default" || c.starts_with("aws:role:")))
                // AWS options need a restated aws:role reference (update_provider).
                || (aws_options && !reference.is_some_and(|c| c.starts_with("aws:role:")))
        } else {
            b.get("endpoint").is_some()
                || b["aws_external_id"].is_string()
                || b["aws_session_name"].is_string()
                || reference.is_some_and(|c| c != "env:CONTRACT_REFERENCE")
        }
    };
    let stored_kind = |connection: Uuid| {
        let pool = pool.clone();
        async move {
            sqlx::query_scalar::<_, String>("SELECT provider FROM provider_connections WHERE id=$1")
                .bind(connection)
                .fetch_one(&pool)
                .await
                .unwrap()
        }
    };
    let (kind_or, kind_bed) = (stored_kind(vprov).await, stored_kind(vbed).await);
    body_set_when(&mut j, a, "PATCH", patch_template, &patch_or, json!({"enabled":true,"credential_ref":"env:CONTRACT_REFERENCE","aws_external_id":null,"aws_session_name":null}), (&|b| refused_by_state(&kind_or, b), &[])).await;
    body_set_when(&mut j, a, "PATCH", patch_template, &patch_bed, json!({"enabled":true,"credential_ref":"aws:role:arn:aws:iam::123456789012:role/gateway/vector","aws_external_id":"tenant-ext-42","aws_session_name":"gateway.vector","endpoint":null}), (&|b| refused_by_state(&kind_bed, b), &[])).await;
    // An endpoint string on the Bedrock connection: the endpoint pattern has no
    // derivable length bounds, so candidates one edit apart (the canonical origin,
    // and the same origin with an uppercase letter the host alphabet refuses).
    body_set_when_candidates(
        &mut j,
        a,
        "PATCH",
        patch_template,
        &patch_bed,
        json!({"enabled":true,"endpoint":"https://bedrock-runtime.us-east-1.amazonaws.com"}),
        (&|b| refused_by_state(&kind_bed, b), &[]),
        &[
            (
                "/endpoint",
                json!("https://bedrock-runtime.us-east-1.amazonaws.com"),
            ),
            (
                "/endpoint",
                json!("https://bedrock-runtime.us-east-1.amazonaws.coM"),
            ),
        ],
    )
    .await;
    // The other request shapes of the provider PATCH: a null reference (keep), an
    // endpoint string, and the positional wire form with each union arm.
    for (path, kind, body) in [
        (
            &patch_or,
            &kind_or,
            json!({"enabled":true,"credential_ref":null}),
        ),
        (
            // Bedrock: an endpoint item (even null) is refused on any other kind.
            &patch_bed,
            &kind_bed,
            json!([true, "aws:default", null, null, null]),
        ),
        (&patch_bed, &kind_bed, json!([true, null, null, null, null])),
        (
            &patch_bed,
            &kind_bed,
            json!([
                true,
                "aws:role:arn:aws:iam::123456789012:role/gateway/vector",
                "tenant-ext-42",
                "gateway.vector",
                null
            ]),
        ),
    ] {
        body_set_when(
            &mut j,
            a,
            "PATCH",
            patch_template,
            path,
            body,
            (&|b| refused_by_state(kind, b), &[]),
        )
        .await;
    }
    body_set(&mut j, a, "POST", "/api/v1/platform/deployments", "/api/v1/platform/deployments", json!({"model_id":vmodel,"provider_connection_id":vprov,"upstream_model":"vector/upstream","enabled":false}), &["route_unsupported_capability"]).await;
    body_set(
        &mut j,
        a,
        "POST",
        "/api/v1/platform/deployments",
        "/api/v1/platform/deployments",
        json!([vmodel, vprov, "vector/upstream-positional", false]),
        &["route_unsupported_capability"],
    )
    .await;
    body_set(
        &mut j,
        a,
        "PATCH",
        "/api/v1/platform/deployments/{id}",
        &format!("/api/v1/platform/deployments/{vdep}"),
        json!({"enabled":true}),
        &[],
    )
    .await;
    body_set(
        &mut j,
        a,
        "PATCH",
        "/api/v1/platform/deployments/{id}",
        &format!("/api/v1/platform/deployments/{vdep}"),
        json!([true]),
        &[],
    )
    .await;
    // A zero input ceiling is valid only when every input-family meter is not applicable.
    body_set_when(
        &mut j,
        a,
        "POST",
        "/api/v1/platform/deployments/{deployment}/prices",
        &format!("/api/v1/platform/deployments/{vdep}/prices"),
        v3_price(),
        (
            &|b| {
                (b["pricing_version"] == 3 && b["input_token_limit"] == 0) || price_lines_refused(b)
            },
            &["price_meters_incomplete"],
        ),
    )
    .await;
    // The other price shapes: version 1 (rates), version 2 (cache pricing) and a
    // version 3 list with explicit unknown lines in place of seven not-applicable
    // lines, plus an unknown audio-token meter.
    let mut unknowns = v3_price();
    let object_meters = [
        "cache_read_tokens",
        "cache_write_tokens",
        "cache_write_5m_tokens",
        "cache_write_1h_tokens",
        "output_images",
        "input_characters",
        "input_audio_seconds_ms",
    ];
    let mut lines: Vec<Value> = unknowns["price_lines"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|l| !object_meters.contains(&l["meter"].as_str().unwrap_or("")))
        .cloned()
        .collect();
    for meter in object_meters {
        lines.push(json!({"meter":meter,"unknown":true}));
    }
    lines.push(json!({"meter":"output_audio_tokens","unknown":true}));
    unknowns["price_lines"] = json!(lines);
    for body in price_variants().into_iter().skip(1).chain([
        json!({"pricing_version":1,"input_microusd_per_million":"1","output_microusd_per_million":"2","input_token_limit":100,"output_token_limit":10}),
        json!({"pricing_version":2,"input_microusd_per_million":"1","output_microusd_per_million":"2","input_token_limit":100,"output_token_limit":10,"cache_pricing":{"read":{"status":"priced","microusd_per_million":"1"},"write":{"status":"unknown"},"write_5m":{"status":"unknown"},"write_1h":{"status":"unknown"}}}),
        unknowns,
    ]) {
        body_set_when(
            &mut j,
            a,
            "POST",
            "/api/v1/platform/deployments/{deployment}/prices",
            &format!("/api/v1/platform/deployments/{vdep}/prices"),
            body,
            (
                &|b| {
                    (b["pricing_version"] == 3 && b["input_token_limit"] == 0)
                        || price_lines_refused(b)
                },
                &["price_meters_incomplete"],
            ),
        )
        .await;
    }
    body_set(&mut j, a, "PUT", "/api/v1/platform/deployments/{deployment}/routing", &format!("/api/v1/platform/deployments/{vdep}/routing"), json!({"priority":1,"weight":2,"residency":"eu","failure_threshold":3,"cooldown_seconds":30}), &[]).await;
    body_set(&mut j, a, "PUT", "/api/v1/platform/models/{model}/routing", &format!("/api/v1/platform/models/{vmodel}/routing"), json!({"strategy":"priority","max_attempts":2,"allow_ambiguous_failover":false,"required_residency":"eu"}), &[]).await;
    // An IANA zone must be known to the server: another length-valid name is a domain refusal.
    // The HH:MM window times have no derivable length bounds: candidates one edit
    // apart (a valid time, the same time missing its last digit).
    body_set_when_candidates(&mut j, a, "PUT", "/api/v1/platform/deployments/{deployment}/batch-scheduling", &format!("/api/v1/platform/deployments/{vdep}/batch-scheduling"), json!({"max_concurrency":3,"yield_live_threshold":5,"window":{"timezone":"UTC","days":["mon","fri"],"start":"23:00","end":"06:00"}}), (&|b| b["window"]["timezone"].as_str().is_some_and(|z| (1..=64).contains(&z.len()) && z != "UTC"), &[]), &[("/window/start", json!("23:00")), ("/window/start", json!("23:0")), ("/window/end", json!("06:00")), ("/window/end", json!("06:0"))]).await;
    body_set(
        &mut j,
        a,
        "POST",
        "/api/v1/platform/deployments/{id}/configuration/preview",
        &format!("/api/v1/platform/deployments/{vdep}/configuration/preview"),
        json!({"enabled":false}),
        &[],
    )
    .await;
    // The positional wire form (and null arms) of the remaining setup operations.
    for (method, template, path, body) in [
        (
            "POST",
            "/api/v1/platform/deployments/{id}/configuration/preview",
            format!("/api/v1/platform/deployments/{vdep}/configuration/preview"),
            json!([false]),
        ),
        (
            "PUT",
            "/api/v1/platform/catalogs/{id}/models",
            format!("/api/v1/platform/catalogs/{vcat}/models"),
            json!([[vmodel]]),
        ),
        (
            "PUT",
            "/api/v1/platform/models/{id}/catalogs",
            format!("/api/v1/platform/models/{vmodel}/catalogs"),
            json!([[vcat]]),
        ),
        (
            "PUT",
            "/api/v1/platform/deployments/{deployment}/routing",
            format!("/api/v1/platform/deployments/{vdep}/routing"),
            json!([1, 2, "eu", 3, 30]),
        ),
        (
            "PUT",
            "/api/v1/platform/deployments/{deployment}/routing",
            format!("/api/v1/platform/deployments/{vdep}/routing"),
            json!({"priority":1,"weight":2,"residency":null,"failure_threshold":3,"cooldown_seconds":30}),
        ),
        (
            "PUT",
            "/api/v1/platform/deployments/{deployment}/routing",
            format!("/api/v1/platform/deployments/{vdep}/routing"),
            json!([1, 2, null, 3, 30]),
        ),
        (
            "PUT",
            "/api/v1/platform/models/{model}/routing",
            format!("/api/v1/platform/models/{vmodel}/routing"),
            json!(["priority", 2, false, "eu"]),
        ),
        (
            "PUT",
            "/api/v1/platform/models/{model}/routing",
            format!("/api/v1/platform/models/{vmodel}/routing"),
            json!({"strategy":"priority","max_attempts":2,"allow_ambiguous_failover":false,"required_residency":null}),
        ),
        (
            "PUT",
            "/api/v1/platform/models/{model}/routing",
            format!("/api/v1/platform/models/{vmodel}/routing"),
            json!(["priority", 2, false, null]),
        ),
        (
            "POST",
            "/api/v1/platform/models",
            "/api/v1/platform/models".to_owned(),
            json!([
                "vector/created-positional",
                "Vector created",
                "A model",
                true,
                ["chat_completions"]
            ]),
        ),
        (
            "POST",
            "/api/v1/platform/models",
            "/api/v1/platform/models".to_owned(),
            json!({"public_name":"vector/created-embeddings","display_name":"Vector created","description":"A model","enabled":true,"supported_protocols":["embeddings"]}),
        ),
        (
            "POST",
            "/api/v1/platform/models",
            "/api/v1/platform/models".to_owned(),
            json!([
                "vector/created-embeddings-positional",
                "Vector created",
                "A model",
                true,
                ["embeddings"]
            ]),
        ),
        (
            "POST",
            "/api/v1/platform/providers",
            "/api/v1/platform/providers".to_owned(),
            json!([
                "Vector Bedrock positional",
                "bedrock",
                "aws:role:arn:aws:iam::123456789012:role/gateway/vector",
                null,
                "us-east-1",
                false,
                "tenant-ext-42",
                "gateway.vector"
            ]),
        ),
    ] {
        body_set(
            &mut j,
            a,
            method,
            template,
            &path,
            body,
            &["route_unsupported_capability"],
        )
        .await;
    }
    // Batch scheduling: a metrics gate in both forms, a priority, the null arms and
    // the positional window. The fixture approves no local endpoint, so a gate's URL
    // is refused by server state (the handler's METRICS_URL message): that 400 is a
    // domain refusal, read from the message (see thresholds_accepted).
    {
        let (client, root) = (j.client.clone(), j.base.clone());
        let path = format!("/api/v1/platform/deployments/{vdep}/batch-scheduling");
        let template = "/api/v1/platform/deployments/{deployment}/batch-scheduling";
        let gate = json!({"url":"http://127.0.0.1:8000/metrics","max_waiting":5,"max_running":null,"max_kv_cache_percent":null});
        let times = |at: &str| -> Vec<(String, Value)> {
            vec![
                (format!("{at}/start"), json!("23:00")),
                (format!("{at}/start"), json!("23:0")),
                (format!("{at}/end"), json!("06:00")),
                (format!("{at}/end"), json!("06:0")),
            ]
        };
        for (valid, candidates) in [
            (
                json!({"max_concurrency":3,"yield_live_threshold":5,"metrics":gate.clone(),"priority":10,"window":null}),
                vec![],
            ),
            (
                json!({"max_concurrency":3,"yield_live_threshold":null,"metrics":["http://127.0.0.1:8000/metrics",null,4,50],"priority":null,"window":["UTC",["mon"],"23:00","06:00"]}),
                vec![
                    ("/window/2".to_owned(), json!("23:00")),
                    ("/window/2".to_owned(), json!("23:0")),
                    ("/window/3".to_owned(), json!("06:00")),
                    ("/window/3".to_owned(), json!("06:0")),
                ],
            ),
            (
                json!([3, 5, null, 10, {"timezone":"UTC","days":["mon","fri"],"start":"23:00","end":"06:00"}]),
                times("/4"),
            ),
            (json!([3, null, gate, null, null]), vec![]),
        ] {
            let candidates: Vec<(&str, Value)> = candidates
                .iter()
                .map(|(p, v)| (p.as_str(), v.clone()))
                .collect();
            let (client, root, path) = (client.clone(), root.clone(), path.clone());
            http_vectors::body_vectors_extended(
                &mut j.observed,
                "PUT",
                template,
                valid,
                &[],
                &candidates,
                async |body: Value| {
                    let schema_valid = schema_accepts("PUT", template, &body);
                    let captured =
                        send(&client, &root, Some(a), "PUT", &path, Some(body.clone())).await;
                    let message = serde_json::from_slice::<Value>(&captured.bytes)
                        .ok()
                        .and_then(|v| v["error"]["message"].as_str().map(str::to_owned));
                    if schema_valid
                        && captured.status == StatusCode::BAD_REQUEST
                        && message.as_deref() == Some(METRICS_URL)
                    {
                        Probe::decided(captured, true)
                    } else {
                        judged(captured, &[])
                    }
                },
            )
            .await;
        }
    }
    {
        // The model is inserted (and may conflict on its unique alias) before the route
        // is validated, so every request names a fresh alias unless the vector changes it.
        let (client, root) = (j.client.clone(), j.base.clone());
        let base = json!({"model":{"public_name":"vector/setup","display_name":"Vector setup","description":"A model","enabled":true,"supported_protocols":["chat_completions"]},"route":{"provider_connection_id":vprov,"upstream_model":"vector/setup","enabled":false},"price":null,"catalog_ids":[vcat]});
        // The nested price walks every price shape too (null, then each variant).
        for price in std::iter::once(Value::Null).chain(price_variants()) {
            let mut valid = base.clone();
            valid["price"] = price;
            let (client, root) = (client.clone(), root.clone());
            http_vectors::body_vectors_http(
                &mut j.observed,
                "POST",
                "/api/v1/platform/model-setup",
                valid,
                async |mut body: Value| {
                    if body["model"]["public_name"] == "vector/setup" {
                        body["model"]["public_name"] =
                            json!(format!("vector/setup-{}", Uuid::new_v4()));
                    }
                    // A nested price whose lines the REAL PriceLines rules refuse
                    // is the documented cross-line refusal (see price_lines_refused).
                    let cross_line = schema_accepts("POST", "/api/v1/platform/model-setup", &body)
                        && price_refused(&body["price"]);
                    let captured = send(
                        &client,
                        &root,
                        Some(a),
                        "POST",
                        "/api/v1/platform/model-setup",
                        Some(body),
                    )
                    .await;
                    if cross_line
                        && matches!(
                            captured.status,
                            StatusCode::BAD_REQUEST | StatusCode::UNPROCESSABLE_ENTITY
                        )
                    {
                        Probe::decided(captured, true)
                    } else {
                        judged(
                            captured,
                            &["route_unsupported_capability", "price_meters_incomplete"],
                        )
                    }
                },
            )
            .await;
        }
    }
    body_set(
        &mut j,
        o,
        "POST",
        "/api/v1/workspaces/{ws}/models",
        &format!("/api/v1/workspaces/{team}/models"),
        json!({"model_id":second}),
        &[],
    )
    .await;
    body_set(
        &mut j,
        o,
        "POST",
        "/api/v1/workspaces/{ws}/models",
        &format!("/api/v1/workspaces/{team}/models"),
        json!([second]),
        &[],
    )
    .await;
    {
        // A fresh team per request: the policy PUT is tighten-only against stored values.
        // One budget per period for all four periods, so the 4-item bound's at-limit
        // vector is this valid body (one budget per period is a cross-item rule the
        // generated variants cannot satisfy).
        let (client, root) = (j.client.clone(), j.base.clone());
        let pool = &pool;
        let owner_id = f.owner.user_id;
        let mut all_periods = policy.clone();
        all_periods["budgets"] = json!([
            {"period":"day","amount_microusd":"1000000"},
            {"period":"week","amount_microusd":"7000000"},
            {"period":"month","amount_microusd":"9007199254740993"},
            ["lifetime","9007199254740993"]
        ]);
        // Every request shape: one budget per period (one item positional), the
        // monthly form, every limit null with no budget, and the positional forms.
        let unlimited = json!({"requests_per_minute":null,"tokens_per_minute":null,"concurrent_requests":null,"concurrent_jobs":null,"storage_bytes":null,"key_issuances_per_holder":null,"key_issuances_per_workspace":null,"monthly_budget_microusd":null});
        let monthly = json!({"requests_per_minute":10,"tokens_per_minute":1000,"concurrent_requests":2,"concurrent_jobs":1,"storage_bytes":10_000,"key_issuances_per_holder":5,"key_issuances_per_workspace":50,"monthly_budget_microusd":"100","budget_period":"month"});
        for valid in [
            all_periods,
            monthly,
            unlimited,
            json!([10, 1000, 2, 1, 10_000, 5, 50, "100", "month"]),
            json!([null, null, null, null, null, null, null, null]),
        ] {
            http_vectors::body_vectors_http(
                &mut j.observed,
                "PUT",
                "/api/v1/workspaces/{ws}/policy",
                valid,
                async |body: Value| {
                    let ws = workspace(pool, "Vector workspace", "team", owner_id).await;
                    // One budget per period is a cross-item rule the schema documents
                    // but cannot state: the REAL parser decides that refusal.
                    let budgets = if body.is_array() {
                        body[9].clone()
                    } else {
                        body["budgets"].clone()
                    };
                    let cross_item = budgets.is_array()
                        && schema_accepts("PUT", "/api/v1/workspaces/{ws}/policy", &body)
                        && !crate::management::governance::budgets_parse(&budgets);
                    let captured = send(
                        &client,
                        &root,
                        Some(o),
                        "PUT",
                        &format!("/api/v1/workspaces/{ws}/policy"),
                        Some(body),
                    )
                    .await;
                    if cross_item && captured.status == StatusCode::BAD_REQUEST {
                        Probe::decided(captured, true)
                    } else {
                        judged(captured, &["exceeds_parent_rate", "exceeds_parent_budget"])
                    }
                },
            )
            .await;
        }
    }
    // ---- Handler-derived vectors the artifact-derived ones cannot generate. ----
    // Each compares the SCHEMA's verdict with the real handler's. `Some((schema,
    // handler))` states a divergence the contract documents instead of expressing.
    let policy_template = "/api/v1/workspaces/{ws}/policy";
    let mut policy_vectors: Vec<(&str, Value, Expected)> = Vec::new();
    let budget = |period: &str, amount: &str| {
        let mut body = policy.clone();
        body["budgets"] = json!([{"period":period,"amount_microusd":amount}]);
        body
    };
    let legacy_budget = |amount: &str| json!({"requests_per_minute":null,"tokens_per_minute":null,"concurrent_requests":null,"monthly_budget_microusd":amount,"budget_period":"month"});
    policy_vectors.extend([
        ("a budget above the signed 64-bit maximum", budget("month", "9223372036854775808"), BOTH_REFUSE),
        ("a zero budget", budget("month", "0"), BOTH_REFUSE),
        ("a budget with a leading zero (contract: canonical only)", budget("month", "0100"), HANDLER_ONLY),
        ("a monthly budget above the signed 64-bit maximum", legacy_budget("9223372036854775808"), BOTH_REFUSE),
        ("a zero monthly budget", legacy_budget("0"), BOTH_REFUSE),
        ("a monthly budget with a leading zero (contract: canonical only)", legacy_budget("0100"), HANDLER_ONLY),
        ("the largest budget of a period without a parent", budget("day", "9223372036854775807"), BOTH_ACCEPT),
        ("two budgets for one period", {
            let mut body = policy.clone();
            body["budgets"] = json!([{"period":"month","amount_microusd":"100"},{"period":"month","amount_microusd":"200"}]);
            body
        }, SCHEMA_ONLY),
        // Exactly one budget form; budget_period only beside the monthly form
        // (Policy::limits: anything else is invalid).
        ("both budget forms with a period", json!({"requests_per_minute":null,"tokens_per_minute":null,"concurrent_requests":null,"budgets":[{"period":"month","amount_microusd":"5"}],"monthly_budget_microusd":"5","budget_period":"month"}), BOTH_REFUSE),
        ("an empty budget list, a null monthly budget and a period", json!({"requests_per_minute":null,"tokens_per_minute":null,"concurrent_requests":null,"budgets":[],"monthly_budget_microusd":null,"budget_period":"month"}), BOTH_REFUSE),
        ("a budget list with a period", json!({"requests_per_minute":null,"tokens_per_minute":null,"concurrent_requests":null,"budgets":[{"period":"month","amount_microusd":"5"}],"budget_period":"month"}), BOTH_REFUSE),
        ("a positional policy with a monthly budget", json!([10, 1000, 2, 1, 10_000, 5, 50, "100", "month"]), BOTH_ACCEPT),
        // The positional form has 8 or 9 items (Policy::limits, as the schema states).
        ("a positional policy without a budget form", json!([10, 1000, 2, 1]), BOTH_REFUSE),
        ("a positional policy with both budget forms", json!([10, 1000, 2, 1, 10_000, 5, 50, "100", "month", [{"period":"day","amount_microusd":"5"}]]), BOTH_REFUSE),
    ]);
    for (label, body, expected) in policy_vectors {
        // A fresh team per request: the policy PUT is tighten-only against stored values.
        let ws = workspace(&pool, "Vector workspace", "team", f.owner.user_id).await;
        extra_body(
            &j,
            o,
            "PUT",
            policy_template,
            &format!("/api/v1/workspaces/{ws}/policy"),
            label,
            body,
            expected,
        )
        .await;
    }
    // Prices: rate bounds, the pair limit, line rules and counts, label controls.
    let prices_template = "/api/v1/platform/deployments/{deployment}/prices";
    let prices = format!("/api/v1/platform/deployments/{vdep}/prices");
    let line_of = |meter: &str, amount: &str| json!({"meter":meter,"microusd_per_batch":amount,"batch":1_000_000,"unit_label":"/M tokens","sku_label":"Rate"});
    let with_lines = |extra: Vec<Value>, drop_output: bool| {
        let mut body = v3_price();
        let mut lines: Vec<Value> = body["price_lines"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|l| !(drop_output && l["meter"] == "output_tokens"))
            .cloned()
            .collect();
        lines.extend(extra);
        body["price_lines"] = json!(lines);
        body
    };
    let tiers = |count: u64| {
        (1..=count)
            .map(|n| tiered(line_of("input_tokens", &(1000 + n).to_string()), n))
            .collect::<Vec<_>>()
    };
    let cache = |read: &str| json!({"pricing_version":2,"input_microusd_per_million":"1","output_microusd_per_million":"2","input_token_limit":100,"output_token_limit":10,"cache_pricing":{"read":{"status":"priced","microusd_per_million":read},"write":{"status":"unknown"},"write_5m":{"status":"unknown"},"write_1h":{"status":"unknown"}}});
    let rates = |input: &str, output: &str, limit: u64, out_limit: u64| json!({"input_microusd_per_million":input,"output_microusd_per_million":output,"input_token_limit":limit,"output_token_limit":out_limit});
    let mut sku = v3_price();
    sku["price_lines"][0]["sku_label"] = json!("In\u{9f}put");
    let mut unknown_line = with_lines(tiers(52), false);
    unknown_line["price_lines"]
        .as_array_mut()
        .unwrap()
        .push(json!({"meter":"input_audio_tokens","unknown":true}));
    let cache_v2 = |input: &str, read: &str, limit: u64| json!({"pricing_version":2,"input_microusd_per_million":input,"output_microusd_per_million":"2","input_token_limit":limit,"output_token_limit":10,"cache_pricing":{"read":{"status":"priced","microusd_per_million":read},"write":{"status":"unknown"},"write_5m":{"status":"unknown"},"write_1h":{"status":"unknown"}}});
    // Version 3: the stated lines replace the same meters of the baseline, with the given
    // ceilings. Two rates of 2.3e15 per million tokens at the largest ceiling each charge
    // about 4.9e18 (fits a signed 64-bit integer); together they do not.
    let v3_set = |set: Vec<Value>, limits: (u64, u64), max_units: Value| {
        let mut body = v3_price();
        let mut lines: Vec<Value> = body["price_lines"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|l| !set.iter().any(|s| s["meter"] == l["meter"]))
            .cloned()
            .collect();
        lines.extend(set);
        body["price_lines"] = json!(lines);
        body["input_token_limit"] = json!(limits.0);
        body["output_token_limit"] = json!(limits.1);
        body["max_units"] = max_units;
        body
    };
    let (huge, large, ceiling) = (
        9_223_372_036_854_775_807_u64,
        2_300_000_000_000_000_u64,
        2_147_483_647_u64,
    );
    // A version 3 price whose batch list mirrors the meters of the standard list.
    let mirrored = || {
        v3_with(
            &[
                line("input_tokens", 1_000_000, 5),
                line("output_tokens", 1_000_000, 6),
            ],
            &[],
            json!({}),
        )
    };
    let batch_edit = |edit: &dyn Fn(&mut Vec<Value>)| {
        let mut body = mirrored();
        let mut lines = body["batch_price_lines"].as_array().unwrap().clone();
        edit(&mut lines);
        body["batch_price_lines"] = json!(lines);
        body
    };
    let mut batch_overflow = mirrored();
    batch_overflow["input_token_limit"] = json!(ceiling);
    for l in batch_overflow["batch_price_lines"].as_array_mut().unwrap() {
        if l["meter"] == "input_tokens" {
            l["microusd_per_batch"] = json!(huge.to_string());
        }
    }
    let mut unknown_only_standard = mirrored();
    unknown_only_standard["price_lines"]
        .as_array_mut()
        .unwrap()
        .push(json!({"meter":"input_audio_tokens","unknown":true}));
    for (label, body, expected) in [
        (
            "an input rate above the signed 64-bit maximum",
            rates("9223372036854775808", "1", 1, 1),
            BOTH_REFUSE,
        ),
        (
            "an input rate with leading zeros",
            rates("0000000000000000100", "1", 1, 1),
            BOTH_ACCEPT,
        ),
        (
            "a zero input ceiling on a version 1 price",
            rates("1", "1", 0, 1),
            BOTH_REFUSE,
        ),
        (
            "a version 1 input charge that overflows alone (contract: the charge is arithmetic)",
            rates("9223372036854775807", "1", 2_147_483_647, 1),
            SCHEMA_ONLY,
        ),
        (
            "a version 1 input charge and output charge that each fit and overflow together (contract: the sum is arithmetic)",
            rates(
                "2300000000000000",
                "2300000000000000",
                2_147_483_647,
                2_147_483_647,
            ),
            SCHEMA_ONLY,
        ),
        (
            "a version 1 charge that fits",
            rates("2300000000000000", "1", 2_147_483_647, 1),
            BOTH_ACCEPT,
        ),
        (
            "a cache rate above the signed 64-bit maximum",
            cache("9223372036854775808"),
            BOTH_REFUSE,
        ),
        (
            "a version 2 cache charge (cache rate x input ceiling) that overflows alone",
            cache_v2("1", "9223372036854775807", 2_147_483_647),
            SCHEMA_ONLY,
        ),
        (
            "a version 2 input charge and cache charge that each fit and overflow together",
            cache_v2("2300000000000000", "2300000000000000", 2_147_483_647),
            SCHEMA_ONLY,
        ),
        (
            "a version 2 cache charge that fits",
            cache_v2("1", "2300000000000000", 2_147_483_647),
            BOTH_ACCEPT,
        ),
        (
            "a version 3 input charge (units x rate) that overflows alone",
            v3_set(
                vec![line("input_tokens", 1_000_000, huge)],
                (ceiling, 10),
                json!({"requests":"1"}),
            ),
            SCHEMA_ONLY,
        ),
        (
            "a version 3 non-token charge (max_units x rate) that overflows alone",
            v3_set(
                vec![line("requests", 1, huge)],
                (100, 10),
                json!({"requests":"2"}),
            ),
            SCHEMA_ONLY,
        ),
        (
            "a version 3 input charge and output charge that each fit and overflow together",
            v3_set(
                vec![
                    line("input_tokens", 1_000_000, large),
                    line("output_tokens", 1_000_000, large),
                ],
                (ceiling, ceiling),
                json!({"requests":"1"}),
            ),
            SCHEMA_ONLY,
        ),
        (
            "a version 3 input charge that fits",
            v3_set(
                vec![line("input_tokens", 1_000_000, large)],
                (ceiling, 10),
                json!({"requests":"1"}),
            ),
            BOTH_ACCEPT,
        ),
        (
            "a batch list that overflows while the standard list does not",
            batch_overflow,
            SCHEMA_ONLY,
        ),
        (
            "a batch list of the same meters at other rates",
            mirrored(),
            BOTH_ACCEPT,
        ),
        (
            "a batch list with a tier the standard list lacks (contract: same_meters compares two properties)",
            batch_edit(&|lines| lines.push(tiered(line("input_tokens", 1_000_000, 7), 3))),
            SCHEMA_ONLY,
        ),
        (
            "a batch list without a meter the standard list prices",
            batch_edit(&|lines| lines.retain(|l| l["meter"] != "output_tokens")),
            SCHEMA_ONLY,
        ),
        (
            "a batch list that states a priced meter not_applicable",
            batch_edit(&|lines| {
                lines.retain(|l| l["meter"] != "output_tokens");
                lines.push(json!({"meter":"output_tokens","not_applicable":true}));
            }),
            SCHEMA_ONLY,
        ),
        (
            "an explicit unknown in the standard list only",
            unknown_only_standard,
            SCHEMA_ONLY,
        ),
        ("a sku label with a C1 control character", sku, BOTH_REFUSE),
        (
            "two lines for one meter and tier (contract: only whole-line duplicates)",
            with_lines(vec![line_of("input_tokens", "999")], false),
            SCHEMA_ONLY,
        ),
        (
            "a not_applicable line next to a priced tier of one meter",
            with_lines(
                vec![
                    tiered(line_of("output_tokens", "5"), 5),
                    json!({"meter":"output_tokens","not_applicable":true}),
                ],
                true,
            ),
            SCHEMA_ONLY,
        ),
        (
            "64 valued lines and an explicit unknown",
            unknown_line,
            BOTH_ACCEPT,
        ),
        // The 64-valued-line cap is a handler rule the schema documents but
        // cannot state (no total item bound): a documented schema-only divergence.
        ("65 valued lines", with_lines(tiers(53), false), SCHEMA_ONLY),
        // The schema states meters as strings; serde also takes the object
        // spelling of a unit variant (documented: stricter than the handler).
        (
            "a meter in serde's object spelling",
            {
                let mut body = v3_price();
                body["price_lines"][0]["meter"] = json!({"input_tokens":null});
                body
            },
            HANDLER_ONLY,
        ),
    ] {
        extra_body(
            &j,
            a,
            "POST",
            prices_template,
            &prices,
            label,
            body,
            expected,
        )
        .await;
    }
    // Routing: the reserved residency is refused by both routing handlers (residency_valid
    // takes lowercase words, so the capitalised spelling is invalid and a look-alike is not
    // the reserved name).
    for (template, path, mut body, field) in [
        (
            "/api/v1/platform/deployments/{deployment}/routing",
            format!("/api/v1/platform/deployments/{vdep}/routing"),
            json!({"priority":1,"weight":2,"residency":"eu","failure_threshold":3,"cooldown_seconds":30}),
            "residency",
        ),
        (
            "/api/v1/platform/models/{model}/routing",
            format!("/api/v1/platform/models/{vmodel}/routing"),
            json!({"strategy":"priority","max_attempts":2,"allow_ambiguous_failover":false,"required_residency":"eu"}),
            "required_residency",
        ),
    ] {
        for (label, value, expected) in [
            ("the reserved residency", "unspecified", BOTH_REFUSE),
            (
                "the reserved residency capitalised",
                "Unspecified",
                BOTH_REFUSE,
            ),
            (
                "a look-alike of the reserved residency",
                "unspecified2",
                BOTH_ACCEPT,
            ),
        ] {
            body[field] = json!(value);
            extra_body(&j, a, "PUT", template, &path, label, body.clone(), expected).await;
        }
    }
    // Scheduling: thresholds are checked before the URL and the fixture server approves no
    // local endpoint, so each vector below is pinned as (schema accepts, THRESHOLDS
    // accepted): the handler's own message says which rule refused (see
    // `thresholds_accepted`), and the rules are run again with the URL approved.
    let schedule_template = "/api/v1/platform/deployments/{deployment}/batch-scheduling";
    let schedule_vdep = format!("/api/v1/platform/deployments/{vdep}/batch-scheduling");
    let gate = |thresholds: Value| {
        let mut metrics = json!({"url":"http://10.0.0.5:8000/metrics","max_waiting":null,"max_running":null,"max_kv_cache_percent":null});
        for (k, v) in thresholds.as_object().unwrap() {
            metrics[k] = v.clone();
        }
        json!({"max_concurrency":3,"metrics":metrics})
    };
    let gates: Vec<(String, Value, Expected)> = [
        ("without a threshold".to_owned(), gate(json!({})), BOTH_REFUSE),
        (
            "with every threshold in range".to_owned(),
            gate(json!({"max_waiting":5,"max_running":6,"max_kv_cache_percent":50})),
            BOTH_ACCEPT,
        ),
        (
            "with one valid threshold and one out of range".to_owned(),
            gate(json!({"max_waiting":5,"max_kv_cache_percent":101})),
            BOTH_REFUSE,
        ),
        (
            "as a positional gate without a threshold (contract: the positional form has no coupling)"
                .to_owned(),
            json!({"max_concurrency":3,"metrics":["http://10.0.0.5:8000/metrics"]}),
            SCHEMA_ONLY,
        ),
        (
            "as a positional gate with a threshold".to_owned(),
            json!({"max_concurrency":3,"metrics":["http://10.0.0.5:8000/metrics", 5]}),
            BOTH_ACCEPT,
        ),
    ]
    .into_iter()
    .chain(
        [
            ("max_waiting", 0, true),
            ("max_waiting", 100_000, true),
            ("max_waiting", 100_001, false),
            ("max_waiting", -1, false),
            ("max_running", 0, true),
            ("max_running", 100_000, true),
            ("max_running", 100_001, false),
            ("max_running", -1, false),
            ("max_kv_cache_percent", 0, false),
            ("max_kv_cache_percent", 1, true),
            ("max_kv_cache_percent", 100, true),
            ("max_kv_cache_percent", 101, false),
            ("max_kv_cache_percent", -1, false),
        ]
        .map(|(field, value, accepted)| {
            let mut threshold = serde_json::Map::new();
            threshold.insert(field.to_owned(), json!(value));
            (
                format!("with {field} {value}"),
                gate(Value::Object(threshold)),
                (accepted, accepted),
            )
        }),
    )
    .collect();
    for (label, body, expected) in gates {
        extra_schedule(
            &j,
            a,
            schedule_template,
            &schedule_vdep,
            &format!("a metrics gate {label}"),
            body,
            expected,
        )
        .await;
    }
    // JSON 3.0 is an integer to JSON Schema but a float to serde.
    extra_body(
        &j,
        a,
        "PUT",
        schedule_template,
        &schedule_vdep,
        "an integer written as 3.0",
        json!({"max_concurrency":3.0}),
        SCHEMA_ONLY,
    )
    .await;
    // Id lists: 200 distinct existing ids are accepted; one more EXISTING id is refused for
    // the bound itself (a nonexistent id would also be refused by the existence check, so
    // it could not show that the cap is what answers).
    let mut bulk_models = vec![];
    let mut bulk_catalogs = vec![];
    for n in 0..200 {
        bulk_models.push(id(&j.request("POST", "/api/v1/platform/models", Some(json!({"public_name":format!("bulk/model-{n}"),"display_name":format!("Bulk model {n}"),"enabled":true}))).await));
        bulk_catalogs.push(id(&j
            .request(
                "POST",
                "/api/v1/platform/catalogs",
                Some(json!({"name":format!("Bulk catalog {n}")})),
            )
            .await));
    }
    let one_more = |ids: &[Uuid], existing: Uuid| {
        let mut ids = ids.to_vec();
        ids.push(existing);
        ids
    };
    for (label, ids, expected) in [
        ("200 distinct ids", bulk_models.clone(), BOTH_ACCEPT),
        (
            "201 distinct existing ids",
            one_more(&bulk_models, second),
            BOTH_REFUSE,
        ),
    ] {
        extra_body(
            &j,
            a,
            "PUT",
            "/api/v1/platform/catalogs/{id}/models",
            &format!("/api/v1/platform/catalogs/{vcat}/models"),
            label,
            json!({"model_ids":ids}),
            expected,
        )
        .await;
    }
    for (label, ids, expected) in [
        ("200 distinct catalogs", bulk_catalogs.clone(), BOTH_ACCEPT),
        (
            "201 distinct existing catalogs",
            one_more(&bulk_catalogs, cat),
            BOTH_REFUSE,
        ),
    ] {
        extra_body(
            &j,
            a,
            "PUT",
            "/api/v1/platform/models/{id}/catalogs",
            &format!("/api/v1/platform/models/{vmodel}/catalogs"),
            label,
            json!({"catalog_ids":ids}),
            expected,
        )
        .await;
        extra_body(
            &j,
            a,
            "PUT",
            "/api/v1/platform/catalog-defaults",
            "/api/v1/platform/catalog-defaults",
            label,
            json!({"personal":ids.clone(),"team":[],"project":[]}),
            expected,
        )
        .await;
        let setup = json!({"model":{"public_name":format!("vector/bulk-{}", Uuid::new_v4()),"display_name":"Bulk setup","enabled":true},"route":{"provider_connection_id":vprov,"upstream_model":"vector/bulk","enabled":false},"catalog_ids":ids});
        extra_body(
            &j,
            a,
            "POST",
            "/api/v1/platform/model-setup",
            "/api/v1/platform/model-setup",
            label,
            setup,
            expected,
        )
        .await;
    }
    // Names are counted in bytes by the handler and in characters by the schema.
    for (label, body, expected) in [
        (
            "a name of 61 two-byte characters",
            json!({"name":"\u{e9}".repeat(61)}),
            SCHEMA_ONLY,
        ),
        (
            "a name of 60 two-byte characters",
            json!({"name":"\u{e9}".repeat(60)}),
            BOTH_ACCEPT,
        ),
        (
            "a description of 1001 two-byte characters",
            json!({"name":"Bytes","description":"\u{e9}".repeat(1001)}),
            SCHEMA_ONLY,
        ),
        (
            "a description of 1000 two-byte characters",
            json!({"name":"Bytes","description":"\u{e9}".repeat(1000)}),
            BOTH_ACCEPT,
        ),
    ] {
        extra_body(
            &j,
            a,
            "POST",
            "/api/v1/platform/catalogs",
            "/api/v1/platform/catalogs",
            label,
            body,
            expected,
        )
        .await;
    }
    // Query values: the search bound counts characters; rate and cursor bounds are exact.
    for (template, path, who) in [
        (
            "/api/v1/platform/catalogs",
            "/api/v1/platform/catalogs".to_owned(),
            a,
        ),
        (
            "/api/v1/workspaces/{ws}/models",
            format!("/api/v1/workspaces/{team}/models"),
            o,
        ),
    ] {
        for (label, count, expected) in [
            ("200 two-byte characters", 200, BOTH_ACCEPT),
            ("201 two-byte characters", 201, BOTH_REFUSE),
        ] {
            extra_query(
                &j,
                who,
                template,
                &path,
                "",
                "q",
                &"\u{e9}".repeat(count),
                label,
                expected,
            )
            .await;
        }
    }
    // Three list searches (catalogs, a workspace's models and its available models) neither
    // trim `q` nor treat a blank one as no filter, unlike the `search()` lists: an empty
    // value lists everything, whitespace lists only names containing it. The contrast is the
    // providers list, which trims.
    let listed = async |j: &Journey<'_>, who: &Browser, template: &str, path: String| {
        let r = send(&j.client, &j.base, Some(who), "GET", &path, None).await;
        assert_eq!(r.status, StatusCode::OK, "GET {template}: list");
        let list: Value = serde_json::from_slice(&r.bytes).unwrap();
        list["data"].as_array().unwrap().len()
    };
    for (template, path, who, trims) in [
        (
            "/api/v1/platform/catalogs",
            "/api/v1/platform/catalogs".to_owned(),
            a,
            false,
        ),
        (
            "/api/v1/workspaces/{ws}/models",
            format!("/api/v1/workspaces/{team}/models"),
            o,
            false,
        ),
        (
            "/api/v1/workspaces/{ws}/available-models",
            format!("/api/v1/workspaces/{team}/available-models"),
            o,
            false,
        ),
        (
            "/api/v1/platform/providers",
            "/api/v1/platform/providers".to_owned(),
            a,
            true,
        ),
    ] {
        let all = listed(&j, who, template, path.clone()).await;
        assert!(all > 0, "GET {template}: nothing to filter");
        assert_eq!(
            listed(&j, who, template, format!("{path}?q=")).await,
            all,
            "GET {template}: an empty q"
        );
        let blank = listed(&j, who, template, format!("{path}?q=%20%20")).await;
        assert_eq!(
            blank,
            if trims { all } else { 0 },
            "GET {template}: a whitespace-only q (trimmed: {trims})"
        );
    }
    for (label, value, expected) in [
        (
            "the signed 64-bit maximum",
            "9223372036854775807",
            BOTH_ACCEPT,
        ),
        ("one above it", "9223372036854775808", BOTH_REFUSE),
        (
            "with leading zeros in 19 digits",
            "0000000000000000100",
            BOTH_ACCEPT,
        ),
        (
            "with leading zeros in 27 digits",
            "000000000000000000000000100",
            BOTH_REFUSE,
        ),
    ] {
        extra_query(
            &j,
            a,
            "/api/v1/platform/models",
            "/api/v1/platform/models",
            "",
            "max_input_price",
            value,
            label,
            expected,
        )
        .await;
    }
    let history_template = "/api/v1/platform/deployments/{id}/configuration/history";
    let row = "00000000-0000-0000-0000-000000000001";
    for (label, value, expected) in [
        (
            "the latest representable instant",
            format!("8210266876799999999_{row}"),
            BOTH_ACCEPT,
        ),
        (
            "one past it",
            format!("8210266876799999999_{row}")
                .replace("8210266876799999999", "8210266876800000000"),
            BOTH_REFUSE,
        ),
        (
            "the earliest representable instant",
            format!("-210866803200000000_{row}"),
            BOTH_ACCEPT,
        ),
        (
            "a signed zero-padded value",
            format!("+0000000000000000000012345_{row}"),
            BOTH_ACCEPT,
        ),
        (
            "a simple uuid",
            "1_00000000000000000000000000000001".to_owned(),
            BOTH_ACCEPT,
        ),
        ("a braced uuid", format!("1_{{{row}}}"), BOTH_ACCEPT),
        ("a urn uuid", format!("1_urn:uuid:{row}"), BOTH_ACCEPT),
        (
            "the i64 maximum",
            format!("9223372036854775807_{row}"),
            BOTH_REFUSE,
        ),
    ] {
        extra_query(
            &j,
            a,
            history_template,
            &history,
            "limit=1",
            "cursor",
            &value,
            label,
            expected,
        )
        .await;
    }
    // Before the PostgreSQL minimum the cursor still parses (cursor_parts only asks chrono),
    // so the query fails in the database and the route answers 503 rather than 400. That
    // is a suspected product bug (logs.rs cursor_parts, tracked in #409); the contract documents the working
    // range only, and this pins the current behaviour so a fix is noticed.
    let early = format!("-210866803200000001_{row}");
    assert!(!param_accepts(history_template, "cursor", &early));
    let r = send(
        &j.client,
        &j.base,
        Some(a),
        "GET",
        &format!("{history}?limit=1&cursor={}", enc(&early)),
        None,
    )
    .await;
    assert_eq!(r.status, StatusCode::SERVICE_UNAVAILABLE);
    // Compare ids: every uuid spelling and Unicode padding the handler parses.
    let compare_template = "/api/v1/platform/models/compare";
    let (m1, m2) = (model.to_string(), second.to_string());
    for (label, value, expected) in [
        (
            "simple spellings",
            format!("{},{}", m1.replace('-', ""), m2.replace('-', "")),
            BOTH_ACCEPT,
        ),
        (
            "braced and urn spellings",
            format!("{{{m1}}},urn:uuid:{m2}"),
            BOTH_ACCEPT,
        ),
        (
            "ideographic space padding",
            format!("\u{3000}{m1}\u{3000},\u{3000}{m2}\u{3000}"),
            BOTH_ACCEPT,
        ),
        (
            "the same id in two spellings (contract: distinctness is not expressible)",
            format!("{m1},{}", m1.replace('-', "")),
            SCHEMA_ONLY,
        ),
        ("one id", m1.clone(), BOTH_REFUSE),
        (
            "five ids",
            format!(
                "{m1},{m2},{},{},{}",
                Uuid::new_v4(),
                Uuid::new_v4(),
                Uuid::new_v4()
            ),
            BOTH_REFUSE,
        ),
    ] {
        extra_query(
            &j,
            a,
            compare_template,
            compare_template,
            "",
            "ids",
            &value,
            label,
            expected,
        )
        .await;
    }
    body_set(
        &mut j,
        a,
        "PUT",
        "/api/v1/platform/catalog-defaults",
        "/api/v1/platform/catalog-defaults",
        json!({"personal":[cat],"team":[cat],"project":[cat]}),
        &[],
    )
    .await;
    body_set(
        &mut j,
        a,
        "PUT",
        "/api/v1/platform/catalog-defaults",
        "/api/v1/platform/catalog-defaults",
        json!([[cat], [cat], [cat]]),
        &[],
    )
    .await;
    j.request(
        "DELETE",
        &format!("/api/v1/platform/deployments/{catalog_only}"),
        None,
    )
    .await;
    j.request(
        "DELETE",
        &format!("/api/v1/platform/deployments/{deployment}"),
        None,
    )
    .await;
    j.request(
        "DELETE",
        &format!("/api/v1/platform/providers/{disposable}"),
        None,
    )
    .await;
    j.request("DELETE", &format!("/api/v1/platform/models/{model}"), None)
        .await;
    j.request("DELETE", &format!("/api/v1/platform/catalogs/{cat}"), None)
        .await;
    let probe_failures = PROBE_FAILURES.with(|failures| failures.take());
    assert!(
        probe_failures.is_empty(),
        "pinned probe verdicts moved:\n{}",
        probe_failures.join("\n")
    );
    j.observed.finish(SLICE3_AREAS);
}
