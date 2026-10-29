//! Slice 4: executed, populated, raw-wire money contracts over a running gateway.
use super::*;
use openapi::responses::{Captured, PositiveFixtures, capture_http};
use sha2::{Digest, Sha256};

#[path = "openapi_money_probes_tests.rs"]
mod probes;
#[path = "openapi_money_vectors_tests.rs"]
mod vectors;

struct Server(tokio::task::JoinHandle<()>);
impl Drop for Server {
    fn drop(&mut self) {
        self.0.abort();
    }
}
struct Browser {
    token: String,
    csrf: String,
}
async fn browser(pool: &PgPool, user: &BrowserPrincipal) -> Browser {
    let token = crate::entropy::hex_token().unwrap();
    let csrf = crate::entropy::hex_token().unwrap();
    sqlx::query("INSERT INTO browser_sessions(token_hash,user_id,csrf_hash,expires_at,verified_email,last_seen_at) VALUES($1,$2,$3,now()+interval '1 hour',$4,now())")
        .bind(Sha256::digest(token.as_bytes()).as_slice()).bind(user.user_id)
        .bind(Sha256::digest(csrf.as_bytes()).as_slice()).bind(&user.email).execute(pool).await.unwrap();
    Browser { token, csrf }
}
async fn send(
    client: &reqwest::Client,
    base: &str,
    actor: Option<&Browser>,
    method: &str,
    path: &str,
    input: Option<Value>,
) -> Captured {
    let mut r = client
        .request(method.parse().unwrap(), format!("{base}{path}"))
        .header("origin", "http://127.0.0.1:3000");
    if let Some(b) = actor {
        r = r
            .header("cookie", format!("omg_session={}", b.token))
            .header("x-csrf-token", &b.csrf);
    }
    if let Some(input) = input {
        r = r.json(&input);
    }
    capture_http(method, path, r.send().await.unwrap()).await
}
fn body(r: &Captured) -> Value {
    // Status and a label only: never the response body (cleartext-logging).
    assert_eq!(r.status, StatusCode::OK, "fixture expected a 200 response");
    serde_json::from_slice(&r.bytes).unwrap()
}

/// The personal workspace's key, named so that a leak of the NAME is detectable.
const PRIVATE_KEY_NAME: &str = "Private personal workspace credential";
/// Key privacy on personal rows (#400 Option A) is enforced by the handlers
/// (the platform `key` dimension collapses personal keys to one redacted row)
/// and asserted here: neither the key's id nor its name may appear.
fn assert_no_personal_key(v: &Value, key: &Value) {
    let text = v.to_string();
    assert!(
        !text.contains(key["id"].as_str().unwrap()),
        "a personal key id is exposed"
    );
    assert!(
        !text.contains(PRIVATE_KEY_NAME),
        "a personal key name is exposed"
    );
}

async fn observe(
    seen: &mut PositiveFixtures,
    client: &reqwest::Client,
    base: &str,
    actor: &Browser,
    method: &str,
    path: &str,
    input: Option<Value>,
) -> Value {
    let r = send(client, base, Some(actor), method, path, input).await;
    let value = body(&r);
    seen.observe(&r);
    value
}

// ---------------------------------------------------------------------------
// Producer-shaped seeding. Rows carry exactly the columns the admission and
// settlement code writes (cost-center snapshot at admission, hold and
// settlement ledger rows, cost components that sum to the settled amount).
// ---------------------------------------------------------------------------
fn ago(days: i64) -> String {
    format!("clock_timestamp()-make_interval(days=>{days})")
}
/// Noon UTC of the day `days` days ago.
fn noon(days: i64) -> String {
    format!(
        "(date_trunc('day',clock_timestamp(),'UTC')-make_interval(days=>{days})+interval '12 hours')"
    )
}
#[derive(Clone, Copy)]
enum Res {
    Settled { actual: i64, held: Option<i64> },
    Unknown { held: Option<i64>, unbounded: bool },
    Pending { held: i64 },
}
#[derive(Clone)]
struct Seed {
    ws: Uuid,
    key: Uuid,
    deployment: Uuid,
    price: Option<Uuid>,
    at: String,
    public_model: &'static str,
    provider: String,
    upstream: Option<&'static str>,
    workload: &'static str,
    /// `None`: an execution that has no reservation row at all.
    res: Option<Res>,
    tokens: Option<(i64, i64)>,
    billing: Option<Value>,
    meter: Option<Value>,
    variant: Option<&'static str>,
    provider_cost: Option<i64>,
    components: Option<Value>,
    redacted: bool,
}
impl Seed {
    fn new(ws: Uuid, key: Uuid, deployment: Uuid, price: Uuid) -> Self {
        Self {
            ws,
            key,
            deployment,
            price: Some(price),
            at: ago(1),
            public_model: "money-contract",
            provider: "anthropic".into(),
            upstream: Some("claude-placeholder"),
            workload: "generation",
            res: Some(Res::Settled {
                actual: 5,
                held: Some(10),
            }),
            tokens: Some((3, 2)),
            billing: None,
            meter: None,
            variant: None,
            provider_cost: None,
            components: None,
            redacted: false,
        }
    }
}
async fn seed(pool: &PgPool, s: &Seed) -> Uuid {
    let id = Uuid::new_v4();
    let pending = matches!(s.res, Some(Res::Pending { .. }));
    let settled = matches!(s.res, Some(Res::Settled { .. }));
    let state = if pending { "started" } else { "succeeded" };
    let (tin, tout) = match (settled, s.tokens) {
        (true, Some((i, o))) => (Some(i), Some(o)),
        _ => (None, None),
    };
    let at = &s.at;
    sqlx::query(&format!("INSERT INTO inference_executions(id,workspace_id,api_key_id,deployment_id,public_model,provider,streamed,state,root_request_id,attempt_number,started_at,input_tokens,output_tokens,billing_usage,meter_usage,output_image_variant,provider_cost_microusd,workload_kind,upstream_model,details_redacted_at,cost_center_id,cost_center_name,cost_center_code) SELECT $1,$2,$3,$4,$5,$6,false,$7,$1,1,{at},$8,$9,$10,$11,$12,$13,$14,$15,CASE WHEN $16 THEN {at} END,w.cost_center_id,c.name,c.code FROM workspaces w LEFT JOIN cost_centers c ON c.id=w.cost_center_id WHERE w.id=$2"))
        .bind(id).bind(s.ws).bind(s.key).bind(s.deployment).bind(s.public_model).bind(&s.provider).bind(state)
        .bind(tin).bind(tout).bind(&s.billing).bind(&s.meter).bind(s.variant).bind(s.provider_cost)
        .bind(s.workload).bind(s.upstream).bind(s.redacted).execute(pool).await.unwrap();
    let Some(res) = s.res else {
        return id;
    };
    let (rstate, held, actual, unbounded) = match res {
        Res::Settled { actual, held } => ("settled", held, Some(actual), false),
        Res::Unknown { held, unbounded } => ("unknown", held, None, unbounded),
        Res::Pending { held } => ("pending", Some(held), None, false),
    };
    let lease = if pending {
        "clock_timestamp()+interval '1 hour'"
    } else {
        "started_at"
    };
    sqlx::query(&format!("INSERT INTO governance_reservations(execution_id,workspace_id,api_key_id,deployment_id,price_id,admitted_at,minute_start,month_start,lease_expires_at,state,held_microusd,actual_microusd,unbounded_cost,input_tokens,output_tokens,billing_usage,cost_components,meter_usage,output_image_variant,provider_cost_microusd) SELECT id,workspace_id,api_key_id,deployment_id,$2,started_at,date_trunc('minute',started_at,'UTC'),date_trunc('month',started_at,'UTC'),{lease},$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13 FROM inference_executions WHERE id=$1"))
        .bind(id).bind(s.price).bind(rstate).bind(held).bind(actual).bind(unbounded).bind(tin).bind(tout)
        .bind(&s.billing).bind(&s.components).bind(&s.meter).bind(s.variant).bind(s.provider_cost)
        .execute(pool).await.unwrap();
    let kind = match rstate {
        "settled" => "settlement",
        "unknown" => "unknown",
        _ => "hold",
    };
    sqlx::query("INSERT INTO monetary_ledger(id,execution_id,admitted_at,kind,amount_microusd,created_at) SELECT $1,execution_id,admitted_at,$3,CASE WHEN $3='hold' THEN held_microusd ELSE actual_microusd END,admitted_at FROM governance_reservations WHERE execution_id=$2")
        .bind(Uuid::new_v4()).bind(id).bind(kind).execute(pool).await.unwrap();
    id
}
async fn seed_attempt(
    pool: &PgPool,
    ws: Uuid,
    key: Uuid,
    deployment: Uuid,
    price: Uuid,
    settled: bool,
    days: i32,
) -> Uuid {
    let res = if settled {
        Res::Settled {
            actual: if days == 1 { 9007199254740993 } else { 5 },
            held: Some(10),
        }
    } else {
        Res::Unknown {
            held: Some(10),
            unbounded: false,
        }
    };
    let s = Seed {
        at: ago(i64::from(days)),
        res: Some(res),
        ..Seed::new(ws, key, deployment, price)
    };
    seed(pool, &s).await
}
const TOKEN_COMPONENTS: [&str; 6] = [
    "uncached_input_microusd",
    "cache_read_microusd",
    "cache_write_default_microusd",
    "cache_write_5m_microusd",
    "cache_write_1h_microusd",
    "output_microusd",
];
const METER_COMPONENTS: [&str; 6] = [
    "output_images_microusd",
    "input_characters_microusd",
    "input_audio_microusd",
    "output_audio_microusd",
    "search_units_microusd",
    "requests_microusd",
];
const AUDIO_COMPONENTS: [&str; 3] = [
    "input_audio_tokens_microusd",
    "cache_read_audio_tokens_microusd",
    "output_audio_tokens_microusd",
];
/// Cost components exactly as settlement stores them (six token keys; twelve
/// with the meter keys; fifteen with the realtime audio-token keys) and the
/// total the reservation's `actual_microusd` must equal.
fn components(tokens: [i64; 6], meters: Option<[i64; 6]>, audio: Option<[i64; 3]>) -> (Value, i64) {
    let mut map = serde_json::Map::new();
    let mut total = 0;
    let mut put = |keys: &[&str], values: &[i64]| {
        for (k, v) in keys.iter().zip(values) {
            map.insert((*k).to_owned(), json!(v.to_string()));
            total += v;
        }
    };
    put(&TOKEN_COMPONENTS, &tokens);
    if let Some(m) = meters {
        put(&METER_COMPONENTS, &m);
    }
    if let Some(a) = audio {
        put(&AUDIO_COMPONENTS, &a);
    }
    (Value::Object(map), total)
}
fn price_line(meter: crate::billing::v3::Meter, amount: &str, batch: u64) -> Value {
    json!({"meter":meter.as_str(),"microusd_per_batch":amount,"batch":batch,"unit_label":meter.unit_label(batch).unwrap(),"sku_label":"Line"})
}
async fn price_v2(pool: &PgPool, deployment: Uuid) -> Uuid {
    let id = Uuid::new_v4();
    let unknown = json!({"status":"unknown"});
    sqlx::query("INSERT INTO deployment_prices(id,deployment_id,input_microusd_per_million,output_microusd_per_million,input_token_limit,output_token_limit,pricing_version,cache_pricing) VALUES($1,$2,1000000,1000000,1000,100,2,$3)")
        .bind(id).bind(deployment)
        .bind(json!({"read":unknown,"write":unknown,"write_5m":unknown,"write_1h":unknown}))
        .execute(pool).await.unwrap();
    id
}
async fn price_v3(pool: &PgPool, deployment: Uuid, audio: bool) -> Uuid {
    use crate::billing::v3::Meter;
    let id = Uuid::new_v4();
    let mut lines: Vec<Value> = Meter::ALL
        .into_iter()
        .map(|m| match m {
            Meter::InputTokens | Meter::OutputTokens | Meter::CacheReadTokens => {
                price_line(m, "1000000", 1_000_000)
            }
            Meter::Requests => price_line(m, "0", 1),
            _ => json!({"meter":m.as_str(),"not_applicable":true}),
        })
        .collect();
    if audio {
        lines.extend(
            Meter::AUDIO_TOKENS
                .into_iter()
                .map(|m| price_line(m, "1000000", 1_000_000)),
        );
    }
    sqlx::query("INSERT INTO deployment_prices(id,deployment_id,input_token_limit,output_token_limit,pricing_version,price_lines,max_units) VALUES($1,$2,1000,100,3,$3,'{}')")
        .bind(id).bind(deployment).bind(json!(lines)).execute(pool).await.unwrap();
    id
}
/// A producer-shaped `billing_usage` with every counter known (uncached 40 +
/// cache read 60 = total 100; no cache writes).
fn billing_full() -> Value {
    json!({"total_input_tokens":"100","uncached_input_tokens":"40","cache_read_input_tokens":"60","cache_write_input_tokens":"0","cache_write_default_input_tokens":"0","cache_write_5m_input_tokens":"0","cache_write_1h_input_tokens":"0"})
}
fn upload_window(start: chrono::NaiveDate, first_amount: &str, supersedes: Option<Uuid>) -> Value {
    let day = |n: i64| format!("{}T00:00:00Z", start + chrono::TimeDelta::days(n));
    let item = |amount: &str, dims: [Option<&str>; 9]| {
        let [
            workspace,
            description,
            model,
            tier,
            window,
            cost_type,
            token_type,
            geo,
            speed,
        ] = dims;
        json!({"amount":amount,"currency":"USD","workspace_id":workspace,"description":description,"model":model,"service_tier":tier,"context_window":window,"cost_type":cost_type,"token_type":token_type,"inference_geo":geo,"speed":speed})
    };
    let bucket = |n: i64, results: Vec<Value>| json!({"starting_at":day(n),"ending_at":day(n+1),"results":results});
    json!({
        "account_id":"organization_placeholder","evidence_reference":"finance:export-window","exported_at":day(4),
        "supersedes":supersedes,
        "request":{"starting_at":day(0),"ending_at":day(4),"bucket_width":"1d","group_by":["workspace_id","description"],"limit":31},
        "pages":[{"data":[
            bucket(0,vec![item(first_amount,[Some("wrkspc_placeholder"),Some("Placeholder model - Input Tokens"),Some("claude-placeholder-alias"),Some("standard"),Some("0-200k"),Some("tokens"),Some("uncached_input_tokens"),Some("global"),Some("standard")])]),
            bucket(1,vec![item("7.5",[None,None,None,None,None,None,None,None,None])]),
            bucket(2,vec![]),
            bucket(3,vec![
                item("3.5",[Some("wrkspc_personal"),Some("Personal tokens"),Some("claude-placeholder"),Some("standard"),Some("0-200k"),Some("tokens"),Some("uncached_input_tokens"),None,None]),
                item("-2.00015",[Some("wrkspc_personal"),None,None,None,None,Some("credits"),None,None,None]),
            ]),
        ],"has_more":false,"next_page":null}]
    })
}
fn window_mapping(
    connection: Uuid,
    deployment: Uuid,
    team: Uuid,
    personal: Uuid,
    start: chrono::NaiveDate,
    supersedes: Uuid,
) -> Value {
    let day = |n: i64| format!("{}T00:00:00Z", start + chrono::TimeDelta::days(n));
    json!({"account_id":"organization_placeholder","supersedes":supersedes,"evidence_reference":"finance:binding-window","bindings":[
        {"provider_workspace_id":"wrkspc_placeholder","connection_id":connection,"deployment_id":deployment,"workspace_id":team,
         "effective_start":day(0),"effective_end":day(1),"exclusive":true,"non_omg_traffic":false,"attested":true,
         "evidence_reference":"finance:attestation-a","model_mappings":[{"provider_model":"claude-placeholder-alias","upstream_model":"claude-placeholder"}]},
        {"provider_workspace_id":null,"connection_id":connection,"deployment_id":null,"workspace_id":null,
         "effective_start":day(1),"effective_end":day(3),"exclusive":false,"non_omg_traffic":true,"attested":true,
         "evidence_reference":"finance:attestation-b","model_mappings":[]},
        {"provider_workspace_id":"wrkspc_personal","connection_id":connection,"deployment_id":null,"workspace_id":personal,
         "effective_start":day(3),"effective_end":day(4),"exclusive":true,"non_omg_traffic":false,"attested":true,
         "evidence_reference":"finance:attestation-d","model_mappings":[]},
    ]})
}
async fn generate_run(f: &Fixture, schedule: Uuid) -> Uuid {
    use crate::leases::{Lease, Leases};
    sqlx::query("UPDATE report_schedules SET state='active' WHERE id=$1")
        .bind(schedule)
        .execute(&f.s.pool)
        .await
        .unwrap();
    sqlx::query("UPDATE report_schedules SET next_period_end=date_trunc('week',clock_timestamp(),'UTC')-interval '7 days',next_run_at=clock_timestamp()-interval '1 day' WHERE id=$1")
        .bind(schedule).execute(&f.s.pool).await.unwrap();
    let leases = Leases::new();
    leases.renew_once(&f.s.pool).await;
    let fence = leases.held(Lease::ReportScheduler).unwrap();
    let claim = crate::scheduled_reports::claim(&f.s, &fence, schedule)
        .await
        .unwrap()
        .unwrap();
    let snapshot = crate::scheduled_reports::snapshot(&f.s, &claim)
        .await
        .unwrap();
    assert!(
        snapshot["totals"]["attempts"]
            .as_str()
            .unwrap()
            .parse::<u64>()
            .unwrap()
            > 0
    );
    assert!(
        crate::scheduled_reports::complete(&f.s, &fence, &claim, &snapshot)
            .await
            .unwrap()
    );
    sqlx::query("SELECT omg_lease_release('report_scheduler',$1,$2)")
        .bind(fence.holder)
        .bind(fence.epoch)
        .execute(&f.s.pool)
        .await
        .unwrap();
    sqlx::query_scalar(
        "SELECT id FROM report_runs WHERE schedule_id=$1 ORDER BY period_end DESC LIMIT 1",
    )
    .bind(schedule)
    .fetch_one(&f.s.pool)
    .await
    .unwrap()
}
fn represented(r: &Captured, seen: &mut PositiveFixtures) -> Value {
    let v = body(r);
    seen.observe(r);
    v
}
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn management_openapi_slice4_money_fixtures(pool: PgPool) {
    let f = fixture(&pool).await;
    let admin = browser(&pool, &f.admin).await;
    let auditor = browser(&pool, &f.auditor).await;
    let owner = browser(&pool, &f.owner).await;
    let member_browser = browser(&pool, &f.member).await;
    // Reconciliation requires both live platform-write and workspace membership.
    member(&pool, f.team, f.admin.user_id, "admin").await;
    let clean = workspace(&pool, "Clean", "team", f.owner.user_id).await;
    let outsider_personal =
        workspace(&pool, "Outsider personal", "personal", f.outsider.user_id).await;
    let model_id = model(&pool, "money-contract").await;
    let model_b = model(&pool, "money-contract-b").await;
    for ws in [f.team, f.personal, f.project, clean] {
        direct(&pool, ws, model_id).await;
    }
    let connection = Uuid::new_v4();
    let deployment = Uuid::new_v4();
    let deployment_b = Uuid::new_v4();
    let price = Uuid::new_v4();
    sqlx::query("INSERT INTO provider_connections(id,name,provider,credential_ref,enabled) VALUES($1,'Money contract','anthropic','env:UNUSED',true)")
        .bind(connection).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO deployments(id,model_id,provider_connection_id,upstream_model,enabled) VALUES($1,$2,$3,'claude-placeholder',true)")
        .bind(deployment).bind(model_id).bind(connection).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO deployments(id,model_id,provider_connection_id,upstream_model,enabled) VALUES($1,$2,$3,'claude-placeholder-b',true)")
        .bind(deployment_b).bind(model_b).bind(connection).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO deployment_prices(id,deployment_id,input_microusd_per_million,output_microusd_per_million,input_token_limit,output_token_limit,pricing_version) VALUES($1,$2,1000000,1000000,1000,100,1)")
        .bind(price).bind(deployment).execute(&pool).await.unwrap();
    let price_b = Uuid::new_v4();
    sqlx::query("INSERT INTO deployment_prices(id,deployment_id,input_microusd_per_million,output_microusd_per_million,input_token_limit,output_token_limit,pricing_version) VALUES($1,$2,1000000,1000000,1000,100,1)")
        .bind(price_b).bind(deployment_b).execute(&pool).await.unwrap();
    let price2 = price_v2(&pool, deployment).await;
    let price3 = price_v3(&pool, deployment, false).await;
    let price_audio = price_v3(&pool, deployment, true).await;
    let (_, team_key) = key(&f, &f.owner, f.team, Value::Null).await;
    // A distinctive name, so a leak of the key NAME (not only its id) is detectable.
    let (_, private_key) = call(
        &f.s,
        &f.owner,
        "POST",
        &format!("/api/v1/workspaces/{}/keys", f.personal),
        json!({"name":PRIVATE_KEY_NAME,"expires_in_days":1,"model_ids":Value::Null}),
    )
    .await;
    let (_, member_key) = key(&f, &f.member, f.team, Value::Null).await;
    let (_, project_key_a) = key(&f, &f.owner, f.project, Value::Null).await;
    let (_, project_key_b) = key(&f, &f.owner, f.project, Value::Null).await;
    let (_, clean_key) = key(&f, &f.owner, clean, Value::Null).await;
    let account = Uuid::new_v4();
    sqlx::query("INSERT INTO service_accounts(id,workspace_id,name) VALUES($1,$2,'Robot')")
        .bind(account)
        .bind(f.team)
        .execute(&pool)
        .await
        .unwrap();
    let (robot_status, robot_key) = call(
        &f.s,
        &f.owner,
        "POST",
        &format!("/api/v1/workspaces/{}/keys", f.team),
        json!({"name":"Robot key","expires_in_days":1,"service_account_id":account}),
    )
    .await;
    assert_eq!(robot_status, StatusCode::OK);

    let mock = crate::identity::tests::MockProvider::start(None).await;
    let identity = mock.state(pool.clone()).await;
    let engine = crate::inference::Engine::new(
        std::sync::Arc::new(f.s.clone()),
        crate::providers::ProviderRegistry::default(),
        crate::inference::EngineLimits::default(),
    )
    .unwrap();
    let app = crate::http::router_with_identity(f.s.clone(), None, engine, identity);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let _server = Server(tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap()
    }));
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let mut seen = PositiveFixtures::default();
    let now = chrono::Utc::now().date_naive();
    let start = (now - chrono::TimeDelta::days(7)).to_string();
    let end = (now + chrono::TimeDelta::days(1)).to_string();
    let range = format!("start_date={start}&end_date={end}");
    let wsbase = format!("/api/v1/workspaces/{}", f.team);
    let projbase = format!("/api/v1/workspaces/{}", f.project);
    let cleanbase = format!("/api/v1/workspaces/{clean}");
    let personalbase = format!("/api/v1/workspaces/{}", f.personal);

    // Empty-state reads, before any attempt exists: nothing aged, no totals.
    let v = observe(
        &mut seen,
        &client,
        &base,
        &auditor,
        "GET",
        "/api/v1/platform/holds",
        None,
    )
    .await;
    assert_eq!(v["totals"]["count"], "0");
    assert!(v["totals"]["oldest_age_hours"].is_null());
    assert!(v["personal"]["oldest_age_hours"].is_null());
    let v = observe(
        &mut seen,
        &client,
        &base,
        &owner,
        "GET",
        &format!("{cleanbase}/holds"),
        None,
    )
    .await;
    assert!(v["totals"]["oldest_age_hours"].is_null());

    // Attempts admitted before any cost center exists carry no snapshot.
    let unallocated_week = seed(
        &pool,
        &Seed {
            at: ago(7),
            ..Seed::new(f.team, id(&team_key), deployment, price)
        },
    )
    .await;
    let (rich, rich_total) = components([4, 1, 0, 0, 0, 2], None, None);
    let _ = unallocated_week;
    seed(
        &pool,
        &Seed {
            at: ago(3),
            price: Some(price2),
            res: Some(Res::Settled {
                actual: rich_total,
                held: Some(10),
            }),
            tokens: Some((100, 20)),
            billing: Some(billing_full()),
            components: Some(rich),
            ..Seed::new(f.team, id(&team_key), deployment, price)
        },
    )
    .await;

    let centers = "/api/v1/platform/cost-centers";
    let cc = id(&observe(
        &mut seen,
        &client,
        &base,
        &admin,
        "POST",
        centers,
        Some(json!({"name":"Admission snapshot","code":"MONEY"})),
    )
    .await);
    let cc_archived = id(&observe(
        &mut seen,
        &client,
        &base,
        &admin,
        "POST",
        centers,
        Some(json!({"name":"Retired label","code":"RETIRED"})),
    )
    .await);
    observe(
        &mut seen,
        &client,
        &base,
        &admin,
        "DELETE",
        &format!("{centers}/{cc_archived}"),
        None,
    )
    .await;
    sqlx::query("UPDATE workspaces SET cost_center_id=$1 WHERE id=$2")
        .bind(cc)
        .bind(f.team)
        .execute(&pool)
        .await
        .unwrap();
    seed_attempt(&pool, f.team, id(&team_key), deployment, price, true, 1).await;
    let unresolved = seed_attempt(&pool, f.team, id(&team_key), deployment, price, false, 3).await;
    let private = seed_attempt(
        &pool,
        f.personal,
        id(&private_key),
        deployment,
        price,
        false,
        3,
    )
    .await;
    sqlx::query("UPDATE governance_reservations SET held_microusd=NULL,unbounded_cost=true WHERE execution_id=$1").bind(private).execute(&pool).await.unwrap();
    // A second personal hold with a known floor, beside the unbounded one.
    seed(
        &pool,
        &Seed {
            at: ago(2),
            res: Some(Res::Unknown {
                held: Some(10),
                unbounded: false,
            }),
            ..Seed::new(f.personal, id(&private_key), deployment, price)
        },
    )
    .await;
    let own = seed_attempt(&pool, f.team, id(&member_key), deployment, price, true, 2).await;
    // Always populate the latest completed weekly period, irrespective of
    // which weekday this test runs on. Settled rows do not grow the hold queue.
    seed_attempt(&pool, f.team, id(&team_key), deployment, price, true, 7).await;
    seed_attempt(
        &pool,
        f.personal,
        id(&private_key),
        deployment,
        price,
        true,
        7,
    )
    .await;
    // v3 meter settlement: every optional billing/meter field known or unknown.
    let (v3, v3_total) = components([2, 0, 0, 0, 0, 1], Some([3, 1, 0, 0, 0, 1]), None);
    seed(
        &pool,
        &Seed {
            at: ago(3),
            price: Some(price3),
            workload: "images",
            res: Some(Res::Settled { actual: v3_total, held: Some(10) }),
            tokens: Some((50, 10)),
            billing: Some(json!({"total_input_tokens":"50","uncached_input_tokens":"50","cache_read_input_tokens":"0","cache_write_input_tokens":"0","cache_write_default_input_tokens":null,"cache_write_5m_input_tokens":null,"cache_write_1h_input_tokens":null})),
            meter: Some(json!({"output_images":"2","input_characters":"1000","input_audio_seconds_ms":"1500","output_audio_seconds_ms":"500","search_units":"3","requests":"1","output_video_seconds_ms":"2000"})),
            variant: Some("1024x1024"),
            provider_cost: Some(12),
            components: Some(v3),
            ..Seed::new(f.team, id(&team_key), deployment, price)
        },
    )
    .await;
    let (audio, audio_total) = components(
        [1, 1, 0, 0, 0, 1],
        Some([0, 0, 2, 1, 0, 1]),
        Some([2, 1, 3]),
    );
    seed(
        &pool,
        &Seed {
            at: ago(3),
            price: Some(price_audio),
            workload: "audio_speech",
            res: Some(Res::Settled { actual: audio_total, held: Some(10) }),
            tokens: Some((30, 30)),
            billing: Some(json!({"total_input_tokens":null,"uncached_input_tokens":null,"cache_read_input_tokens":null,"cache_write_input_tokens":null,"cache_write_default_input_tokens":null,"cache_write_5m_input_tokens":null,"cache_write_1h_input_tokens":null})),
            meter: Some(json!({"output_images":null,"input_characters":null,"input_audio_seconds_ms":"3000","output_audio_seconds_ms":null,"search_units":null,"requests":"1"})),
            components: Some(audio),
            ..Seed::new(f.team, id(&team_key), deployment, price)
        },
    )
    .await;
    let (v3b, v3b_total) = components([1, 0, 0, 0, 0, 1], Some([1, 0, 0, 1, 0, 0]), None);
    seed(
        &pool,
        &Seed {
            at: ago(3),
            price: Some(price3),
            workload: "images",
            res: Some(Res::Settled {
                actual: v3b_total,
                held: Some(10),
            }),
            tokens: Some((5, 5)),
            meter: Some(json!({"output_images":"1","input_characters":null,"input_audio_seconds_ms":null,"output_audio_seconds_ms":"700","search_units":null,"requests":null,"output_video_seconds_ms":null})),
            components: Some(v3b),
            ..Seed::new(f.team, id(&team_key), deployment, price)
        },
    )
    .await;
    // An unbounded hold (no known floor), an attempt with no reservation row
    // at all, a settled attempt whose details were compacted, and a robot
    // (service account) settlement. Compaction (maintenance.rs:40) redacts only
    // SETTLED executions, so the missing-reservation row is never redacted.
    // (A settled row carries its compaction exactly as the job writes it: the
    // execution is stamped with details_redacted_at and nothing else changes.)
    seed(
        &pool,
        &Seed {
            at: ago(2),
            res: Some(Res::Unknown {
                held: None,
                unbounded: true,
            }),
            ..Seed::new(f.team, id(&team_key), deployment, price)
        },
    )
    .await;
    seed(
        &pool,
        &Seed {
            at: ago(2),
            price: None,
            res: None,
            ..Seed::new(f.team, id(&team_key), deployment, price)
        },
    )
    .await;
    seed(
        &pool,
        &Seed {
            at: ago(2),
            redacted: true,
            ..Seed::new(f.team, id(&team_key), deployment, price)
        },
    )
    .await;
    seed(
        &pool,
        &Seed {
            at: ago(2),
            ..Seed::new(f.team, id(&robot_key), deployment, price)
        },
    )
    .await;
    // One public alias served by two different models over time.
    for (dep, pr) in [(deployment, price), (deployment_b, price_b)] {
        seed(
            &pool,
            &Seed {
                at: ago(2),
                public_model: "money-renamed",
                ..Seed::new(f.team, id(&team_key), dep, pr)
            },
        )
        .await;
    }
    // More than 100 providers in the last completed week truncate the
    // scheduled snapshot's provider breakdown.
    for n in 0..101 {
        seed(
            &pool,
            &Seed {
                at: ago(7),
                provider: format!("bulk-{n:03}"),
                ..Seed::new(f.team, id(&team_key), deployment, price)
            },
        )
        .await;
    }
    // Project: in-flight attempts only (pending reservations, no usage yet).
    for k in [&project_key_a, &project_key_b] {
        seed(
            &pool,
            &Seed {
                at: "clock_timestamp()-interval '2 hours'".into(),
                res: Some(Res::Pending { held: 10 }),
                tokens: None,
                ..Seed::new(f.project, id(k), deployment, price)
            },
        )
        .await;
    }
    // Clean workspace: settled usage only.
    for _ in 0..2 {
        seed(
            &pool,
            &Seed {
                at: ago(2),
                ..Seed::new(clean, id(&clean_key), deployment, price)
            },
        )
        .await;
    }
    // Provider-window history (12..9 days ago, UTC noon): day A is clean team
    // usage, day C has attempts the import does not claim, day D is personal.
    let window_start = now - chrono::TimeDelta::days(12);
    let (window_v2, window_total) = components([4, 1, 0, 0, 0, 2], None, None);
    for (at, ws, key_id, pr, res) in [
        (
            noon(12),
            f.team,
            id(&team_key),
            price2,
            Res::Settled {
                actual: window_total,
                held: Some(10),
            },
        ),
        (
            noon(12),
            f.team,
            id(&team_key),
            price,
            Res::Settled {
                actual: 5,
                held: Some(10),
            },
        ),
        (
            noon(10),
            f.team,
            id(&team_key),
            price,
            Res::Settled {
                actual: 5,
                held: Some(10),
            },
        ),
        (
            noon(9),
            f.personal,
            id(&private_key),
            price,
            Res::Settled {
                actual: 5,
                held: Some(10),
            },
        ),
    ] {
        let rich = pr == price2;
        seed(
            &pool,
            &Seed {
                at,
                price: Some(pr),
                res: Some(res),
                billing: rich.then(billing_full),
                components: rich.then(|| window_v2.clone()),
                tokens: Some(if rich { (100, 20) } else { (3, 2) }),
                ..Seed::new(ws, key_id, deployment, price)
            },
        )
        .await;
    }
    seed(
        &pool,
        &Seed {
            at: noon(10),
            price: None,
            res: None,
            upstream: None,
            ..Seed::new(f.team, id(&team_key), deployment, price)
        },
    )
    .await;
    for ws in [f.team, f.personal, f.project, outsider_personal] {
        sqlx::query("INSERT INTO storage_usage_hours(workspace_id,purpose,hour_start,byte_seconds,file_count) VALUES($1,'user_file',date_trunc('hour',clock_timestamp())-interval '1 day',3600000,1)")
            .bind(ws).execute(&pool).await.unwrap();
    }
    // Platform Admins removed the project cap and one personal cap.
    sqlx::query("UPDATE workspace_type_policies SET storage_bytes=NULL WHERE kind='project'")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO workspace_platform_policy_overrides(workspace_id) VALUES($1)")
        .bind(outsider_personal)
        .execute(&pool)
        .await
        .unwrap();

    for p in [
        centers.to_owned(),
        format!("{centers}/{cc}"),
        format!("{centers}/{cc_archived}"),
    ] {
        let v = observe(&mut seen, &client, &base, &auditor, "GET", &p, None).await;
        if p == centers {
            assert!(!v["data"].as_array().unwrap().is_empty());
        }
    }
    observe(
        &mut seen,
        &client,
        &base,
        &admin,
        "PATCH",
        &format!("{centers}/{cc}"),
        Some(json!({"name":"Renamed label","code":"RENAMED"})),
    )
    .await;
    for (actor, scope) in [
        (&auditor, "/api/v1/platform".to_owned()),
        (&owner, wsbase.clone()),
    ] {
        let platform = scope.contains("/platform");
        for suffix in ["cost-report", "usage/overview", "usage/storage"] {
            let v = observe(
                &mut seen,
                &client,
                &base,
                actor,
                "GET",
                &format!("{scope}/{suffix}?{range}"),
                None,
            )
            .await;
            if suffix == "cost-report" {
                assert!(
                    v["totals"]["attempts"]
                        .as_str()
                        .unwrap()
                        .parse::<u64>()
                        .unwrap()
                        > 0
                );
                assert_eq!(v["breakdowns_truncated"], true);
                assert!(v["provider_reported_cost_microusd"].is_string());
                assert!(v["comparison"].is_null());
            }
            if suffix == "usage/overview" && platform {
                assert!(
                    v["top"]["keys"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|r| r["id"].is_null() && r["name"] == "Personal workspace keys")
                );
                assert_no_personal_key(&v["top"]["keys"], &private_key);
            }
            if suffix == "usage/storage" {
                assert_ne!(v["total"]["byte_seconds"], "0");
            }
        }
        // Previous-period comparison, and every filter narrowing the report.
        let v = observe(
            &mut seen,
            &client,
            &base,
            actor,
            "GET",
            &format!("{scope}/cost-report?{range}&compare=previous_period"),
            None,
        )
        .await;
        assert!(v["comparison"]["totals"].is_object());
        observe(
            &mut seen,
            &client,
            &base,
            actor,
            "GET",
            &format!("{scope}/cost-report?{range}&model=money-contract&provider=anthropic&accounting_status=settled&cost_center_id={cc}&status=succeeded"),
            None,
        )
        .await;
        observe(
            &mut seen,
            &client,
            &base,
            actor,
            "GET",
            &format!("{scope}/cost-report?{range}&cost_center_id=unallocated"),
            None,
        )
        .await;
        // Storage: both capped and uncapped workspaces.
        observe(
            &mut seen,
            &client,
            &base,
            actor,
            "GET",
            &format!("{scope}/usage/storage?{range}"),
            None,
        )
        .await;
    }
    for ws in [&projbase, &personalbase] {
        observe(
            &mut seen,
            &client,
            &base,
            &owner,
            "GET",
            &format!("{ws}/usage/storage?{range}"),
            None,
        )
        .await;
    }
    // Scopes whose attempts carry no billing detail yet.
    observe(
        &mut seen,
        &client,
        &base,
        &owner,
        "GET",
        &format!("{projbase}/cost-report?{range}"),
        None,
    )
    .await;
    observe(
        &mut seen,
        &client,
        &base,
        &auditor,
        "GET",
        &format!(
            "/api/v1/platform/cost-report?{range}&workspace_id={}",
            f.project
        ),
        None,
    )
    .await;
    // A window holding only the personal workspace's unsettled attempts: its
    // collapsed key row has no spend share and no blended rate.
    let recent = (now - chrono::TimeDelta::days(5)).to_string();
    let v = observe(
        &mut seen,
        &client,
        &base,
        &auditor,
        "GET",
        &format!(
            "/api/v1/platform/usage/overview?start_date={recent}&end_date={end}&workspace_id={}",
            f.personal
        ),
        None,
    )
    .await;
    assert!(v["top"]["keys"][0]["share"].is_null());
    assert_no_personal_key(&v, &private_key);
    // Usage overview: busy scope, a scope with only in-flight attempts, the
    // unallocated bucket, and a member (own activity only: no member list).
    for (actor, scope, filter) in [
        (
            &auditor,
            "/api/v1/platform".to_owned(),
            format!("&workspace_id={}", f.project),
        ),
        (
            &auditor,
            "/api/v1/platform".to_owned(),
            "&cost_center_id=unallocated".to_owned(),
        ),
        (&owner, projbase.clone(), String::new()),
        (
            &owner,
            wsbase.clone(),
            "&cost_center_id=unallocated".to_owned(),
        ),
        (&member_browser, wsbase.clone(), String::new()),
    ] {
        let v = observe(
            &mut seen,
            &client,
            &base,
            actor,
            "GET",
            &format!("{scope}/usage/overview?{range}{filter}"),
            None,
        )
        .await;
        if actor.token == member_browser.token {
            assert!(v["top"]["members"].is_null());
        }
        if scope.contains("/platform") {
            assert_no_personal_key(&v, &private_key);
        }
    }
    // Usage explore: every metric, grouping and then-grouping, including the
    // totals-of-zero, top-N overflow and personal-key collapse paths.
    let platform_explore = [
        "metric=cache_hit_rate&group_by=key".to_owned(),
        "metric=cache_hit_rate&group_by=cost_center".to_owned(),
        format!(
            "metric=cache_hit_rate&group_by=key&workspace_id={}",
            f.project
        ),
        "metric=cache_hit_rate&group_by=key&then_by=cost_center".to_owned(),
        "metric=cache_hit_rate&group_by=cost_center&then_by=key".to_owned(),
        format!(
            "metric=cache_hit_rate&group_by=model&then_by=workspace&workspace_id={}",
            f.project
        ),
        "metric=spend&group_by=key".to_owned(),
        "metric=spend&group_by=key&top=1".to_owned(),
        "metric=spend&group_by=cost_center".to_owned(),
        format!(
            "metric=tokens&group_by=key&top=1&workspace_id={}",
            f.project
        ),
        "metric=spend&group_by=key&then_by=cost_center&top=1".to_owned(),
        "metric=spend&group_by=cost_center&then_by=key".to_owned(),
        format!(
            "metric=tokens&group_by=key&then_by=model&top=1&workspace_id={}",
            f.project
        ),
        "metric=requests&group_by=day".to_owned(),
        "metric=spend&group_by=member&then_by=workspace".to_owned(),
        // Personal workspaces as the SECOND dimension: identity and totals only.
        "metric=spend&group_by=model&then_by=workspace".to_owned(),
        "metric=requests&group_by=workspace".to_owned(),
    ];
    for query in &platform_explore {
        let v = observe(
            &mut seen,
            &client,
            &base,
            &auditor,
            "GET",
            &format!("/api/v1/platform/usage/explore?{range}&{query}"),
            None,
        )
        .await;
        assert_no_personal_key(&v, &private_key);
        if query.starts_with("metric=requests&group_by=workspace") {
            assert!(v["rows"].as_array().unwrap().iter().any(|r| {
                r["group"]["name"]
                    .as_str()
                    .is_some_and(|n| n.starts_with("Personal \u{b7} "))
            }));
        }
        if query == "metric=spend&group_by=model&then_by=workspace" {
            assert!(v["rows"].as_array().unwrap().iter().any(|r| {
                r["then"]["name"]
                    .as_str()
                    .is_some_and(|n| n.starts_with("Personal \u{b7} "))
            }));
        }
        if query == "metric=spend&group_by=key" {
            assert!(
                v["rows"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|r| r["group"]["id"].is_null()
                        && r["group"]["name"] == "Personal workspace keys")
            );
        }
        if query == "metric=spend&group_by=key&top=1" {
            assert!(!v["other"].is_null());
        }
    }
    // Key privacy for BOTH platform actors (the Auditor loops above and the
    // Admin here): every platform view that groups or lists keys exposes
    // neither the personal workspace's key id nor its name.
    for actor in [&auditor, &admin] {
        for path in [
            format!("/api/v1/platform/cost-report?{range}"),
            format!("/api/v1/platform/usage/overview?{range}"),
            format!("/api/v1/platform/usage/explore?{range}&metric=spend&group_by=key"),
            format!(
                "/api/v1/platform/usage/explore?{range}&metric=spend&group_by=key&then_by=cost_center"
            ),
            format!(
                "/api/v1/platform/usage/explore?{range}&metric=spend&group_by=cost_center&then_by=key"
            ),
            format!("/api/v1/platform/usage/explore?{range}&metric=requests&group_by=workspace"),
            "/api/v1/platform/holds".to_owned(),
        ] {
            let v = observe(&mut seen, &client, &base, actor, "GET", &path, None).await;
            assert_no_personal_key(&v, &private_key);
        }
    }
    let member_key_id = id(&member_key).to_string();
    let workspace_explore: Vec<(String, String)> = vec![
        (wsbase.clone(), "metric=cache_hit_rate&group_by=key".into()),
        (
            wsbase.clone(),
            "metric=cache_hit_rate&group_by=cost_center".into(),
        ),
        (
            wsbase.clone(),
            format!("metric=cache_hit_rate&group_by=key&key_id={member_key_id}"),
        ),
        (
            wsbase.clone(),
            "metric=cache_hit_rate&group_by=key&then_by=cost_center".into(),
        ),
        (
            wsbase.clone(),
            "metric=cache_hit_rate&group_by=cost_center&then_by=key".into(),
        ),
        (
            wsbase.clone(),
            format!("metric=cache_hit_rate&group_by=model&then_by=key&key_id={member_key_id}"),
        ),
        (wsbase.clone(), "metric=spend&group_by=key&top=1".into()),
        (wsbase.clone(), "metric=spend&group_by=cost_center".into()),
        (projbase.clone(), "metric=tokens&group_by=key&top=1".into()),
        (
            wsbase.clone(),
            "metric=spend&group_by=key&then_by=cost_center&top=1".into(),
        ),
        (
            wsbase.clone(),
            "metric=spend&group_by=cost_center&then_by=key".into(),
        ),
        (
            projbase.clone(),
            "metric=tokens&group_by=key&then_by=model&top=1".into(),
        ),
        (wsbase.clone(), "metric=requests&group_by=day".into()),
        (
            wsbase.clone(),
            "metric=spend&group_by=member&then_by=model".into(),
        ),
    ];
    for (scope, query) in &workspace_explore {
        observe(
            &mut seen,
            &client,
            &base,
            &owner,
            "GET",
            &format!("{scope}/usage/explore?{range}&{query}"),
            None,
        )
        .await;
    }
    observe(
        &mut seen,
        &client,
        &base,
        &auditor,
        "GET",
        "/api/v1/platform/overview",
        None,
    )
    .await;
    for suffix in [
        "cost-summary".to_owned(),
        "usage".to_owned(),
        format!("costs?{range}&limit=200"),
        format!("costs?{range}&limit=1"),
    ] {
        let v = observe(
            &mut seen,
            &client,
            &base,
            &owner,
            "GET",
            &format!("{wsbase}/{suffix}"),
            None,
        )
        .await;
        if suffix.starts_with("costs?") && suffix.ends_with("200") {
            let rows = v["data"].as_array().unwrap();
            assert!(!rows.is_empty());
            // Snapshots are taken at admission: assigned rows keep the name
            // they had then; rows admitted before the assignment have none.
            assert!(rows.iter().all(|r| r["cost_center_name"].is_null()
                || (r["cost_center_name"] == "Admission snapshot"
                    && r["cost_center_code"] == "MONEY")));
            assert!(rows.iter().any(|r| r["cost_center_id"].is_null()));
            assert!(rows.iter().any(|r| r["cost_microusd"].is_null()));
            assert!(rows.iter().any(|r| r["details_redacted_at"].is_string()));
        }
    }
    observe(
        &mut seen,
        &client,
        &base,
        &owner,
        "GET",
        &format!("{cleanbase}/usage"),
        None,
    )
    .await;
    // Member own activity is filtered before grouping and pagination.
    let v = observe(
        &mut seen,
        &client,
        &base,
        &member_browser,
        "GET",
        &format!("{wsbase}/costs?{range}"),
        None,
    )
    .await;
    assert_eq!(v["data"].as_array().unwrap().len(), 1);
    assert_eq!(v["data"][0]["id"], json!(own));
    for actor in [&admin, &auditor] {
        let v = observe(
            &mut seen,
            &client,
            &base,
            actor,
            "GET",
            "/api/v1/platform/holds",
            None,
        )
        .await;
        assert_eq!(v["personal"]["count"], "2");
        assert_eq!(v["personal"]["known_floor_microusd"], "10");
        assert_eq!(v["personal"]["unbounded_count"], "1");
        for secret in [private, f.personal] {
            assert!(!v.to_string().contains(&secret.to_string()));
        }
        assert_no_personal_key(&v, &private_key);
        assert_eq!(v["data"][0]["can_reconcile"], actor.token == admin.token);
        assert!(
            v["data"]
                .as_array()
                .unwrap()
                .iter()
                .any(|r| r["held_microusd"].is_null())
        );
    }
    let v = observe(
        &mut seen,
        &client,
        &base,
        &auditor,
        "GET",
        "/api/v1/platform/holds?limit=1",
        None,
    )
    .await;
    assert!(v["next_cursor"].is_string());
    let v = observe(
        &mut seen,
        &client,
        &base,
        &owner,
        "GET",
        &format!("{wsbase}/holds"),
        None,
    )
    .await;
    assert_eq!(v["data"][0]["execution_id"], json!(unresolved));
    assert!(v["personal"].is_null());
    let v = observe(
        &mut seen,
        &client,
        &base,
        &owner,
        "GET",
        &format!("{wsbase}/holds?limit=1"),
        None,
    )
    .await;
    assert!(v["next_cursor"].is_string());
    let v = observe(
        &mut seen,
        &client,
        &base,
        &owner,
        "GET",
        &format!("/api/v1/workspaces/{}/holds", f.personal),
        None,
    )
    .await;
    // The owner's own personal holds: exactly the two seeded ones (the
    // unbounded hold and the floor-10 hold), none of the shared workspaces'.
    let rows = v["data"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().any(|r| r["execution_id"] == json!(private)));
    assert!(
        rows.iter()
            .all(|r| r["workspace"]["kind"] == "personal"
                && r["workspace"]["id"] == json!(f.personal))
    );
    assert!(v["personal"].is_null());
    // The owner reads their own personal workspace through every workspace-
    // scoped report; Platform Admins and Auditors get 404 on each of them (they
    // see the workspace's identity and totals only through the platform scope).
    let personal_reads = [
        format!("{personalbase}/cost-report?{range}"),
        format!("{personalbase}/costs?{range}"),
        format!("{personalbase}/usage"),
        format!("{personalbase}/usage/explore?{range}&metric=spend&group_by=key"),
        format!("{personalbase}/usage/overview?{range}"),
    ];
    let own_key = id(&private_key).to_string();
    for path in &personal_reads {
        let v = observe(&mut seen, &client, &base, &owner, "GET", path, None).await;
        if path.contains("/cost-report?") {
            assert!(
                v["totals"]["attempts"]
                    .as_str()
                    .unwrap()
                    .parse::<u64>()
                    .unwrap()
                    > 0
            );
        }
        if path.contains("/costs?") {
            assert!(
                v["data"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|r| r["id"] == json!(private))
            );
        }
        if path.contains("/usage/explore?") {
            assert!(
                v["rows"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|r| r["group"]["id"] == json!(own_key))
            );
        }
        if path.contains("/usage/overview?") {
            assert!(v.to_string().contains(&own_key));
        }
        for actor in [&admin, &auditor] {
            assert_eq!(
                send(&client, &base, Some(actor), "GET", path, None)
                    .await
                    .status,
                StatusCode::NOT_FOUND
            );
        }
    }
    let r = send(
        &client,
        &base,
        Some(&owner),
        "GET",
        &format!("{wsbase}/usage-export?{range}"),
        None,
    )
    .await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(
        std::str::from_utf8(&r.bytes)
            .unwrap()
            .contains("Admission snapshot")
    );
    seen.observe(&r);
    observe(&mut seen,&client,&base,&admin,"POST",&format!("{wsbase}/costs/{unresolved}/reconcile"),Some(json!({"input_tokens":"3","output_tokens":"2","billing_usage":null,"evidence":"contract:receipt"}))).await;
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT actual_microusd FROM governance_reservations WHERE execution_id=$1"
        )
        .bind(unresolved)
        .fetch_one(&pool)
        .await
        .unwrap(),
        5
    );

    // Report schedules: every scope and kind, before and after a run.
    for (actor, scope, recipient, with_cost_center, legacy) in [
        (
            &admin,
            "/api/v1/platform".to_owned(),
            f.admin.user_id,
            true,
            true,
        ),
        (&owner, wsbase.clone(), f.owner.user_id, true, true),
        (&owner, personalbase.clone(), f.owner.user_id, false, false),
    ] {
        let schedules = format!("{scope}/report-schedules");
        let create = |cost_center: Option<Uuid>| json!({"cadence":"weekly","timezone":"UTC","delivery_time":"00:01","recipient_user_ids":[recipient],"cost_center_id":cost_center});
        let schedule = id(&observe(
            &mut seen,
            &client,
            &base,
            actor,
            "POST",
            &schedules,
            Some(create(None)),
        )
        .await);
        let path = format!("{schedules}/{schedule}");
        observe(
            &mut seen,
            &client,
            &base,
            actor,
            "PATCH",
            &path,
            Some(json!({"delivery_time":"00:02"})),
        )
        .await;
        observe(&mut seen, &client, &base, actor, "GET", &path, None).await;
        let mut second = None;
        if with_cost_center {
            let s2 = id(&observe(
                &mut seen,
                &client,
                &base,
                actor,
                "POST",
                &schedules,
                Some(create(Some(cc))),
            )
            .await);
            observe(
                &mut seen,
                &client,
                &base,
                actor,
                "GET",
                &format!("{schedules}/{s2}"),
                None,
            )
            .await;
            observe(
                &mut seen,
                &client,
                &base,
                actor,
                "PATCH",
                &format!("{schedules}/{s2}"),
                Some(json!({"delivery_time":"00:03"})),
            )
            .await;
            second = Some(s2);
        }
        if legacy {
            // A schedule created before the calendar cursor existed has no
            // next due time until the worker initialises it.
            let s3 = id(&observe(
                &mut seen,
                &client,
                &base,
                actor,
                "POST",
                &schedules,
                Some(create(None)),
            )
            .await);
            sqlx::query("UPDATE report_schedules SET next_run_at=NULL,next_period_end=NULL WHERE id=$1 AND state='paused'")
                .bind(s3)
                .execute(&pool)
                .await
                .unwrap();
            observe(
                &mut seen,
                &client,
                &base,
                actor,
                "GET",
                &format!("{schedules}/{s3}"),
                None,
            )
            .await;
        }
        let run = generate_run(&f, schedule).await;
        if let Some(s2) = second {
            let run2 = generate_run(&f, s2).await;
            let v = observe(
                &mut seen,
                &client,
                &base,
                actor,
                "GET",
                &format!("{schedules}/{s2}/runs/{run2}"),
                None,
            )
            .await;
            assert_eq!(v["expired"], false);
            // Run, snapshot and delivery are written in one transaction
            // (scheduled_reports.rs:306-313) and deliveries are never deleted:
            // every run carries its envelope, in the list as well.
            assert!(v["delivery"].is_object());
            let v = observe(
                &mut seen,
                &client,
                &base,
                actor,
                "GET",
                &format!("{schedules}/{s2}/runs"),
                None,
            )
            .await;
            assert!(v["data"][0]["delivery"].is_object());
        }
        // Pause again (an active schedule needs the operator public URL).
        observe(
            &mut seen,
            &client,
            &base,
            actor,
            "PATCH",
            &path,
            Some(json!({"state":"paused","delivery_time":"00:04"})),
        )
        .await;
        observe(&mut seen, &client, &base, actor, "GET", &path, None).await;
        let v = observe(&mut seen, &client, &base, actor, "GET", &schedules, None).await;
        assert!(!v["data"].as_array().unwrap().is_empty());
        let first_id = v["data"][0]["id"].as_str().unwrap().to_owned();
        let v = observe(
            &mut seen,
            &client,
            &base,
            actor,
            "GET",
            &format!("{schedules}?limit=1"),
            None,
        )
        .await;
        if with_cost_center {
            assert!(v["next_cursor"].is_string());
            observe(
                &mut seen,
                &client,
                &base,
                actor,
                "GET",
                &format!("{schedules}?limit=1&after={first_id}"),
                None,
            )
            .await;
        }
        // A creator whose authority lapsed leaves an outcome without a period.
        sqlx::query("INSERT INTO report_generation_outcomes(schedule_id,run_id,configuration_revision,claim_token,period_start,period_end,category) SELECT id,NULL,configuration_revision,NULL,NULL,NULL,'authority_lost' FROM report_schedules WHERE id=$1")
            .bind(schedule)
            .execute(&pool)
            .await
            .unwrap();
        let v = observe(
            &mut seen,
            &client,
            &base,
            actor,
            "GET",
            &format!("{path}/runs"),
            None,
        )
        .await;
        assert!(!v["data"].as_array().unwrap().is_empty());
        assert!(!v["outcomes"].as_array().unwrap().is_empty());
        let runpath = format!("{path}/runs/{run}");
        let v = observe(&mut seen, &client, &base, actor, "GET", &runpath, None).await;
        assert_eq!(v["expired"], false);
        assert_eq!(v["snapshot"]["held_is_floor"], true);
        assert!(!v["delivery"].is_null());
        assert!(
            !v["snapshot"]["truncated_dimensions"]
                .as_array()
                .unwrap()
                .is_empty()
                || !with_cost_center
        );
        if scope.contains("/platform") {
            assert!(!v["snapshot"].to_string().contains(&f.personal.to_string()));
            assert!(
                v["snapshot"]["breakdowns"]["workspaces"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|r| r["id"].is_null() && r["name"] == "Personal (aggregate)")
            );
        }
        let run_schema = json!({"$ref":if scope.contains("/platform") {"#/components/schemas/MoneyPlatformReportRun"} else {"#/components/schemas/MoneyWorkspaceReportRun"}});
        let mut wrong = v.clone();
        wrong["snapshot"] = Value::Null;
        assert!(
            openapi::responses::validate_value(openapi::responses::artifact(), &run_schema, &wrong)
                .is_err()
        );
        let mut wrong = v.clone();
        wrong["expired"] = json!(true);
        assert!(
            openapi::responses::validate_value(openapi::responses::artifact(), &run_schema, &wrong)
                .is_err()
        );
        let r = send(
            &client,
            &base,
            Some(actor),
            "GET",
            &format!("{runpath}/csv"),
            None,
        )
        .await;
        assert_eq!(r.status, StatusCode::OK);
        assert!(std::str::from_utf8(&r.bytes).unwrap().lines().count() > 1);
        seen.observe(&r);
        // Test-only time travel in this fresh fixture: production keeps the
        // occurrence append-only. Restore the trigger before any HTTP read.
        let mut tx = pool.begin().await.unwrap();
        sqlx::query("ALTER TABLE report_runs DISABLE TRIGGER report_runs_immutable")
            .execute(&mut *tx)
            .await
            .unwrap();
        sqlx::query(
            "UPDATE report_runs SET expires_at=clock_timestamp()-interval '1 second' WHERE id=$1",
        )
        .bind(run)
        .execute(&mut *tx)
        .await
        .unwrap();
        sqlx::query("ALTER TABLE report_runs ENABLE TRIGGER report_runs_immutable")
            .execute(&mut *tx)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        let v = observe(&mut seen, &client, &base, actor, "GET", &runpath, None).await;
        assert_eq!(v["expired"], true);
        assert!(v["snapshot"].is_null());
        let r = send(
            &client,
            &base,
            Some(actor),
            "GET",
            &format!("{runpath}/csv"),
            None,
        )
        .await;
        assert_eq!(r.status, StatusCode::GONE);
        observe(&mut seen, &client, &base, actor, "DELETE", &path, None).await;
        assert_eq!(
            send(&client, &base, Some(actor), "GET", &runpath, None)
                .await
                .status,
            StatusCode::NOT_FOUND
        );
    }
    // Provider upload uses actual bytes, preserving duplicate-key evidence.
    let imports = "/api/v1/platform/provider-cost-imports";
    let mappings = "/api/v1/platform/provider-cost-mappings";
    let reconciliations = "/api/v1/platform/provider-reconciliations";
    seed_attempt(
        &pool,
        f.personal,
        id(&private_key),
        deployment,
        price,
        false,
        1,
    )
    .await;
    let mut upload = provider_cost_tests::upload();
    let from = format!("{}T00:00:00Z", now - chrono::TimeDelta::days(1));
    let until = format!("{now}T00:00:00Z");
    upload["request"]["starting_at"] = json!(from);
    upload["request"]["ending_at"] = json!(until);
    upload["exported_at"] = json!(until);
    upload["pages"][0]["data"][0]["starting_at"] = json!(from);
    upload["pages"][0]["data"][0]["ending_at"] = json!(until);
    let mut credit = upload["pages"][0]["data"][0]["results"][0].clone();
    credit["amount"] = json!("-2.00015");
    credit["cost_type"] = json!("credits");
    upload["pages"][0]["data"][0]["results"]
        .as_array_mut()
        .unwrap()
        .push(credit);
    let r = capture_http(
        "POST",
        imports,
        client
            .post(format!("{base}{imports}"))
            .header("origin", "http://127.0.0.1:3000")
            .header("cookie", format!("omg_session={}", admin.token))
            .header("x-csrf-token", &admin.csrf)
            .header("content-type", "application/json")
            .body(serde_json::to_vec(&upload).unwrap())
            .send()
            .await
            .unwrap(),
    )
    .await;
    let import = id(&represented(&r, &mut seen));
    let mapping = id(&observe(
        &mut seen,
        &client,
        &base,
        &admin,
        "POST",
        mappings,
        Some({
            let mut mapping = provider_cost_tests::mapping(connection, Some(f.team));
            mapping["bindings"][0]["effective_start"] = json!(from);
            mapping["bindings"][0]["effective_end"] = json!(until);
            mapping
        }),
    )
    .await);
    let recon = id(&observe(
        &mut seen,
        &client,
        &base,
        &admin,
        "POST",
        reconciliations,
        Some(json!({"import_id":import,"mapping_id":mapping})),
    )
    .await);
    // A multi-day window: a revised import supersedes the first, and a revised
    // mapping binds an exclusive team, a shared non-OMG scope and a personal
    // workspace (whose identity Platform Admins may see; never its requests).
    let import_first = id(&observe(
        &mut seen,
        &client,
        &base,
        &admin,
        "POST",
        imports,
        Some(upload_window(window_start, "123.78912", None)),
    )
    .await);
    let import_revised = id(&observe(
        &mut seen,
        &client,
        &base,
        &admin,
        "POST",
        imports,
        Some(upload_window(window_start, "130.5", Some(import_first))),
    )
    .await);
    let mapping_revised = id(&observe(
        &mut seen,
        &client,
        &base,
        &admin,
        "POST",
        mappings,
        Some(window_mapping(
            connection,
            deployment,
            f.team,
            f.personal,
            window_start,
            mapping,
        )),
    )
    .await);
    let recon_revised = id(&observe(
        &mut seen,
        &client,
        &base,
        &admin,
        "POST",
        reconciliations,
        Some(json!({"import_id":import_revised,"mapping_id":mapping_revised})),
    )
    .await);
    for path in [
        imports.to_owned(),
        format!("{imports}/{import}"),
        format!("{imports}/{import}/lines"),
        format!("{imports}/{import_first}"),
        format!("{imports}/{import_first}/lines"),
        format!("{imports}/{import_revised}"),
        format!("{imports}/{import_revised}/lines"),
        mappings.to_owned(),
        format!("{mappings}/{mapping}"),
        format!("{mappings}/{mapping_revised}"),
        reconciliations.to_owned(),
        format!("{reconciliations}/{recon}"),
        format!("{reconciliations}/{recon_revised}"),
    ] {
        let v = observe(&mut seen, &client, &base, &auditor, "GET", &path, None).await;
        if let Some(data) = v.get("data") {
            assert!(!data.as_array().unwrap().is_empty());
        }
        if path.ends_with(&recon.to_string()) {
            assert!(v["snapshot"]["estimates"]["holds_upper_bound_microusd"].is_null());
            assert!(!v["snapshot"]["rows"].as_array().unwrap().is_empty());
            assert!(v["snapshot"]["rows"].as_array().unwrap().iter().any(|r| {
                r["imported_microusd"]
                    .as_str()
                    .is_some_and(|s| s.starts_with('-'))
            }));
            assert_eq!(
                v["snapshot"]["personal_activity"]["estimates"]["attempts"],
                "1"
            );
            for secret in [f.personal, private] {
                assert!(!v["snapshot"].to_string().contains(&secret.to_string()));
            }
            assert_no_personal_key(&v["snapshot"], &private_key);
        }
        if path.ends_with(&recon_revised.to_string()) {
            let categories =
                &v["snapshot"]["personal_activity"]["exclusively_attributed_imported_by_category"];
            assert!(
                categories["credits"]
                    .as_str()
                    .is_some_and(|s| s.starts_with('-'))
            );
            assert!(categories["tokens"].is_string());
            assert!(
                !v["snapshot"]["rows"]
                    .to_string()
                    .contains(&f.personal.to_string())
            );
            assert!(
                !v["snapshot"]["mapping_provenance"]
                    .to_string()
                    .contains(&f.personal.to_string())
            );
            assert_no_personal_key(&v["snapshot"]["rows"], &private_key);
            assert_no_personal_key(&v["snapshot"]["mapping_provenance"], &private_key);
        }
    }
    let duplicate = serde_json::to_string(&upload).unwrap().replace(
        "\"next_page\":null",
        "\"next_page\":null,\"next_page\":null",
    );
    let r = capture_http(
        "POST",
        imports,
        client
            .post(format!("{base}{imports}"))
            .header("origin", "http://127.0.0.1:3000")
            .header("cookie", format!("omg_session={}", admin.token))
            .header("x-csrf-token", &admin.csrf)
            .header("content-type", "application/json")
            .body(duplicate)
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    observe(
        &mut seen,
        &client,
        &base,
        &admin,
        "DELETE",
        &format!("{centers}/{cc}"),
        None,
    )
    .await;
    // HTTP boundary vectors: every query parameter and request body, compared
    // with the real handler over HTTP. Refusals are collected, not fatal.
    let new_schedule = |recipient: Uuid| json!({"cadence":"weekly","timezone":"UTC","delivery_time":"00:01","recipient_user_ids":[recipient]});
    let platform_schedule = id(&observe(
        &mut seen,
        &client,
        &base,
        &admin,
        "POST",
        "/api/v1/platform/report-schedules",
        Some(new_schedule(f.admin.user_id)),
    )
    .await);
    let workspace_schedule = id(&observe(
        &mut seen,
        &client,
        &base,
        &owner,
        "POST",
        &format!("{wsbase}/report-schedules"),
        Some(new_schedule(f.owner.user_id)),
    )
    .await);
    let vx = vectors::Vx {
        client: &client,
        base: &base,
        pool: &pool,
        f: &f,
        admin: &admin,
        auditor: &auditor,
        owner: &owner,
        start: start.clone(),
        end: end.clone(),
        team: f.team,
        cc,
        key: id(&team_key),
        account,
        model_id,
        deployment,
        price,
        connection,
        import: import_first,
        mapping: mapping_revised,
        platform_schedule,
        workspace_schedule,
        failures: Default::default(),
    };
    vx.queries(&mut seen).await;
    vx.bodies(&mut seen).await;
    vx.probes().await;
    for line in vx.failures.borrow().iter() {
        eprintln!("slice4 vector failure: {line}");
    }
    // Real refusals: unauthenticated, inference credential, foreign personal and Auditor writes.
    assert_eq!(
        send(&client, &base, None, "GET", centers, None)
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );
    let r = capture_http(
        "GET",
        centers,
        client
            .get(format!("{base}{centers}"))
            .bearer_auth(team_key["token"].as_str().unwrap())
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    for actor in [&admin, &auditor] {
        assert_eq!(
            send(
                &client,
                &base,
                Some(actor),
                "GET",
                &format!("/api/v1/workspaces/{}/costs?{range}", f.personal),
                None
            )
            .await
            .status,
            StatusCode::NOT_FOUND
        );
    }
    assert_eq!(
        send(
            &client,
            &base,
            Some(&auditor),
            "POST",
            centers,
            Some(json!({"name":"Denied","code":"DENIED"}))
        )
        .await
        .status,
        StatusCode::FORBIDDEN
    );
    // Real extractor refusal controls follow successful controls for each DTO.
    let schedule_body = json!({"cadence":"weekly","timezone":"UTC","delivery_time":"00:01","recipient_user_ids":[f.admin.user_id],"unexpected_configuration":true});
    for (path, input, status) in [
        (
            centers.to_owned(),
            json!({"name":"Nope","code":"NOPE","unexpected_configuration":true}),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            "/api/v1/platform/report-schedules".to_owned(),
            schedule_body,
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            mappings.to_owned(),
            json!({"account_id":"account","evidence_reference":"receipt","bindings":[{"unexpected_configuration":true}]}),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            reconciliations.to_owned(),
            json!({"import_id":import,"mapping_id":mapping,"unexpected_configuration":true}),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            format!("{wsbase}/costs/{unresolved}/reconcile"),
            json!({"input_tokens":"3","output_tokens":"2","evidence":"receipt"}),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            format!("{wsbase}/costs/{unresolved}/reconcile"),
            json!({"input_tokens":"3","output_tokens":"2","billing_usage":{"unexpected_configuration":true},"evidence":"receipt"}),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
    ] {
        assert_eq!(
            send(&client, &base, Some(&admin), "POST", &path, Some(input))
                .await
                .status,
            status
        );
    }
    let r = capture_http(
        "POST",
        centers,
        client
            .post(format!("{base}{centers}"))
            .header("origin", "http://127.0.0.1:3000")
            .header("cookie", format!("omg_session={}", admin.token))
            .json(&json!({"name":"No CSRF","code":"NO-CSRF"}))
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    seen.finish(openapi::responses::SLICE4_AREAS);
    assert!(vx.failures.borrow().is_empty());
}
