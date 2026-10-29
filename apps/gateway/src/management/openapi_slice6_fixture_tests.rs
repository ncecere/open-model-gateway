//! Slice 6 executed, populated wire fixtures against a running loopback gateway.
//!
//! Data is real: keys, policies, uploads, holds and tokens go through the real
//! API; request, accounting, realtime and job rows are written as their
//! producers write them (`seed`). Failure messages name only the method, a
//! static label and the status, never a path, id, header or body.
use super::*;
use futures::FutureExt;
use openapi::http_vectors::{Probe, body_vectors_http, query_vectors_fixed};
use openapi::responses::{Captured, PositiveFixtures, capture_http};
use sha2::{Digest, Sha256};
#[path = "openapi_slice6_seed_tests.rs"]
mod seed;
use seed::*;

const ORIGIN: &str = "http://127.0.0.1:3000";
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
async fn browser(pool: &PgPool, u: &BrowserPrincipal) -> Browser {
    let token = crate::entropy::hex_token().unwrap();
    let csrf = crate::entropy::hex_token().unwrap();
    sqlx::query("INSERT INTO browser_sessions(token_hash,user_id,csrf_hash,expires_at,verified_email,last_seen_at) VALUES($1,$2,$3,now()+interval '1 hour',$4,now())")
        .bind(Sha256::digest(token.as_bytes()).as_slice()).bind(u.user_id)
        .bind(Sha256::digest(csrf.as_bytes()).as_slice()).bind(&u.email).execute(pool).await.unwrap();
    Browser { token, csrf }
}
/// A multipart form; `typed` gives the file part a content type.
fn multipart(fields: &[(&str, &str)], file: Option<&[u8]>, typed: bool) -> Vec<u8> {
    let mut body = Vec::new();
    for (name, value) in fields {
        body.extend(
            format!(
                "--contract\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"
            )
            .into_bytes(),
        );
    }
    if let Some(file) = file {
        let kind = if typed {
            "Content-Type: text/plain\r\n"
        } else {
            ""
        };
        body.extend(format!("--contract\r\nContent-Disposition: form-data; name=\"file\"; filename=\"contract.txt\"\r\n{kind}\r\n").into_bytes());
        body.extend(file);
        body.extend(b"\r\n");
    }
    body.extend(b"--contract--\r\n");
    body
}
/// One multipart upload to `path` as `b`, validated against the contract.
async fn upload(
    w: &mut Wire,
    b: &Browser,
    path: &str,
    fields: &[(&str, &str)],
    file: Option<&[u8]>,
    typed: bool,
) -> Captured {
    let r = w
        .client
        .post(format!("{}{path}", w.base))
        .header("origin", ORIGIN)
        .header("cookie", format!("omg_session={}", b.token))
        .header("x-csrf-token", &b.csrf)
        .header("content-type", "multipart/form-data; boundary=contract")
        .body(multipart(fields, file, typed))
        .send()
        .await
        .expect("loopback request failed");
    let r = capture_http("POST", path, r).await;
    w.observed.observe(&r);
    r
}
/// One request over loopback, validated against the contract, never observed.
async fn raw(
    client: &reqwest::Client,
    base: &str,
    b: &Browser,
    method: &str,
    path: &str,
    body: Option<Value>,
) -> Captured {
    let mut r = client
        .request(method.parse().unwrap(), format!("{base}{path}"))
        .header("origin", ORIGIN)
        .header("cookie", format!("omg_session={}", b.token))
        .header("x-csrf-token", &b.csrf);
    if let Some(body) = body {
        r = r.json(&body);
    }
    capture_http(
        method,
        path,
        r.send().await.expect("loopback request failed"),
    )
    .await
}
/// A domain refusal (a policy or issuance rule) is not an input refusal, even
/// though its status is 400/403/429: the handler accepted the INPUT.
fn domain_refusal(c: &Captured) -> bool {
    let reason = serde_json::from_slice::<Value>(&c.bytes)
        .ok()
        .and_then(|v| v["error"]["reason"].as_str().map(str::to_owned));
    reason.is_some_and(|r| {
        r == "key_issuance_limit_exceeded"
            || crate::management::POLICY_REASONS
                .iter()
                .any(|(_, reason, _, _)| *reason == r)
    })
}
fn probe(c: Captured) -> Probe {
    if domain_refusal(&c) {
        Probe::decided(c, true)
    } else {
        Probe::from(c)
    }
}
/// Whether the handler got past input validation (the verdict of `Probe`).
fn handler_accepts(c: &Captured, label: &str) -> bool {
    if domain_refusal(c) {
        return true;
    }
    match c.status.as_u16() {
        400 | 422 => false,
        200..=399 | 404 | 409 => true,
        other => panic!("vector {label}: status {other} is not an input verdict"),
    }
}
/// The documented JSON request schema of an operation.
fn request_schema(method: &str, template: &str) -> &'static Value {
    let schema = &openapi::responses::artifact()["paths"][template][method.to_ascii_lowercase()]["requestBody"]
        ["content"]["application/json"]["schema"];
    assert!(!schema.is_null(), "{method}: request schema not found");
    schema
}
/// The schema of a documented query parameter.
fn query_schema(template: &str, name: &str) -> &'static Value {
    let document = openapi::responses::artifact();
    document["paths"][template]["get"]["parameters"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|p| p["in"] == "query" && p["name"] == name)
        .map(|p| &p["schema"])
        .next()
        .expect("documented query parameter")
}
/// Percent-encode every byte but the unreserved ones.
fn encode(value: &str) -> String {
    value
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            other => format!("%{other:02X}"),
        })
        .collect()
}
/// One explicit vector: the contract's verdict equals the handler's, or (for a
/// rule JSON Schema cannot express, `expressible == false`) the contract
/// accepts what the handler refuses, pinning that documented gap.
fn same_verdict(label: &str, schema_accepts: bool, handler: bool, expressible: bool) {
    if expressible {
        assert_eq!(
            schema_accepts, handler,
            "vector {label}: schema and handler disagree"
        );
    } else {
        assert!(
            schema_accepts && !handler,
            "vector {label}: the documented gap changed"
        );
    }
}
#[allow(clippy::too_many_arguments)]
async fn body_vector(
    w: &Wire,
    b: &Browser,
    method: &str,
    template: &str,
    path: &str,
    label: &str,
    body: Value,
    expressible: bool,
) {
    let schema_accepts = openapi::responses::validate_value(
        openapi::responses::artifact(),
        request_schema(method, template),
        &body,
    )
    .is_ok();
    let r = raw(&w.client, &w.base, b, method, path, Some(body)).await;
    same_verdict(
        label,
        schema_accepts,
        handler_accepts(&r, label),
        expressible,
    );
}
#[allow(clippy::too_many_arguments)]
async fn query_vector(
    w: &Wire,
    b: &Browser,
    template: &str,
    path: &str,
    name: &str,
    label: &str,
    value: &str,
    expressible: bool,
) {
    let schema_accepts = openapi::responses::validate_value(
        openapi::responses::artifact(),
        query_schema(template, name),
        &json!(value),
    )
    .is_ok();
    let r = raw(
        &w.client,
        &w.base,
        b,
        "GET",
        &format!("{path}?{name}={}", encode(value)),
        None,
    )
    .await;
    same_verdict(
        label,
        schema_accepts,
        handler_accepts(&r, label),
        expressible,
    );
}
struct Wire {
    client: reqwest::Client,
    base: String,
    observed: PositiveFixtures,
    /// Operations whose vector run failed (the run continues: one build, every gap).
    failed: Vec<String>,
}
impl Wire {
    async fn send(
        &mut self,
        b: &Browser,
        method: &str,
        path: &str,
        body: Option<Value>,
    ) -> Captured {
        let r = raw(&self.client, &self.base, b, method, path, body).await;
        self.observed.observe(&r);
        r
    }
    /// A success; collections must be populated.
    async fn json(&mut self, b: &Browser, method: &str, path: &str, body: Option<Value>) -> Value {
        let v = self.json_any(b, method, path, body).await;
        if let Some(data) = v.get("data") {
            assert!(
                !data.as_array().unwrap().is_empty(),
                "{method}: a populated fixture collection was empty"
            );
        }
        v
    }
    /// A success whose collections may be empty (a filtered read).
    async fn json_any(
        &mut self,
        b: &Browser,
        method: &str,
        path: &str,
        body: Option<Value>,
    ) -> Value {
        let r = self.send(b, method, path, body).await;
        assert!(r.status.is_success(), "{method}: status {}", r.status);
        serde_json::from_slice(&r.bytes).unwrap()
    }
    async fn status(
        &mut self,
        b: &Browser,
        method: &str,
        path: &str,
        body: Option<Value>,
        want: StatusCode,
    ) {
        let r = self.send(b, method, path, body).await;
        assert_eq!(r.status, want, "{method}");
    }
    /// Prerequisite state through the real API of ANOTHER slice: checked as a
    /// success, never counted for this slice's coverage.
    async fn setup(
        &self,
        b: &Browser,
        what: &str,
        method: &str,
        path: &str,
        body: Option<Value>,
    ) -> Value {
        let r = raw(&self.client, &self.base, b, method, path, body).await;
        assert!(r.status.is_success(), "setup {what}: status {}", r.status);
        serde_json::from_slice(&r.bytes).unwrap_or(Value::Null)
    }
}
/// Query boundary vectors of one read, sent as `b` to `path`.
async fn query_ops(w: &mut Wire, b: &Browser, template: &str, path: &str, base: &[(&str, &str)]) {
    let (client, root) = (w.client.clone(), w.base.clone());
    let failed = std::panic::AssertUnwindSafe(query_vectors_fixed(
        &mut w.observed,
        "GET",
        template,
        base,
        async |query: String| raw(&client, &root, b, "GET", &format!("{path}?{query}"), None).await,
    ))
    .catch_unwind()
    .await
    .is_err();
    if failed {
        w.failed.push(format!("GET {template}"));
    }
}
/// Body boundary vectors of one JSON write.
async fn body_ops(
    w: &mut Wire,
    b: &Browser,
    method: &str,
    template: &str,
    path: &str,
    valid: Value,
) {
    let (client, root) = (w.client.clone(), w.base.clone());
    let failed = std::panic::AssertUnwindSafe(body_vectors_http(
        &mut w.observed,
        method,
        template,
        valid,
        async |body: Value| probe(raw(&client, &root, b, method, path, Some(body)).await),
    ))
    .catch_unwind()
    .await
    .is_err();
    if failed {
        w.failed.push(format!("{method} {template}"));
    }
}

/// An async job (kind `batch` or `video`) over an existing execution `root`,
/// created as the job creators create it (`queued`, one upstream or none).
#[allow(clippy::too_many_arguments)]
async fn job(
    pool: &PgPool,
    kind: &str,
    ws: Uuid,
    key: Uuid,
    deployment: Uuid,
    root: Uuid,
    mode: Option<&str>,
    file: Option<Uuid>,
    user: Option<Uuid>,
    tier: Option<&str>,
    counts: Option<(i32, Option<i32>, Option<i32>)>,
) -> Uuid {
    let id = Uuid::new_v4();
    let upstream = (mode != Some("gateway")).then(|| format!("up-{}", id.simple()));
    let video = kind == "video";
    let (total, completed, failed) = counts.map_or((None, None, None), |(t, c, f)| (Some(t), c, f));
    sqlx::query("INSERT INTO async_jobs(id,kind,workspace_id,api_key_id,deployment_id,execution_id,public_model,provider,upstream_id,poll_deadline_at,batch_endpoint,batch_mode,user_id,input_file_id,work_file_id,price_tier,request_total,request_completed,request_failed,video_seconds,video_size) VALUES($1,$2,$3,$4,$5,$6,'contract-log','openai',$7,now()+interval '1 hour',CASE WHEN $8 THEN NULL ELSE '/v1/chat/completions' END,$9,$10,$11,$11,$12,$13,$14,$15,CASE WHEN $8 THEN 5 END,CASE WHEN $8 THEN '1280x720' END)")
        .bind(id).bind(kind).bind(ws).bind(key).bind(deployment).bind(root).bind(upstream).bind(video).bind(mode).bind(user).bind(file).bind(tier).bind(total).bind(completed).bind(failed)
        .execute(pool).await.unwrap();
    id
}
async fn line(
    pool: &PgPool,
    ws: Uuid,
    job: Uuid,
    n: i32,
    state: &str,
    code: Option<&str>,
    deployment: Option<Uuid>,
) {
    sqlx::query("INSERT INTO batch_lines(job_id,workspace_id,line_no,state,status_code,error_code,finished_at,deployment_id) VALUES($1,$2,$3,$4,$5,$6,CASE WHEN $4='running' THEN NULL ELSE clock_timestamp() END,$7)")
        .bind(job).bind(ws).bind(n).bind(state).bind(code.map(|_| 400_i16)).bind(code).bind(deployment)
        .execute(pool).await.unwrap();
}
/// A scheduling wait is fresh for ten seconds: touch it before reading.
async fn fresh_waits(pool: &PgPool) {
    sqlx::query("UPDATE batch_route_waits SET updated_at=clock_timestamp()")
        .execute(pool)
        .await
        .unwrap();
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn management_openapi_slice6_fixtures(pool: PgPool) {
    let f = fixture(&pool).await;
    let owner = browser(&pool, &f.owner).await;
    let member = browser(&pool, &f.member).await;
    let admin = browser(&pool, &f.admin).await;
    let auditor = browser(&pool, &f.auditor).await;
    let rt = crate::filestore::FileStoreRuntime::memory();
    sqlx::query(
        "UPDATE installation_settings SET file_batch_enabled=true,file_user_files_enabled=true",
    )
    .execute(&pool)
    .await
    .unwrap();
    // Models: one served by two connections, five more to make access broad.
    let m = model(&pool, "contract-log").await;
    direct(&pool, f.team, m).await;
    direct(&pool, f.personal, m).await;
    direct(&pool, f.project, m).await;
    for i in 1..=5 {
        let extra = model(&pool, &format!("contract-extra-{i}")).await;
        direct(&pool, f.team, extra).await;
    }
    let (connection, deployment) = (Uuid::new_v4(), Uuid::new_v4());
    sqlx::query("INSERT INTO provider_connections(id,name,provider,credential_ref,enabled) VALUES($1,'Contract connection','openai','env:UNUSED',true)").bind(connection).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO deployments(id,model_id,provider_connection_id,upstream_model,enabled) VALUES($1,$2,$3,'contract-upstream',true)").bind(deployment).bind(m).bind(connection).execute(&pool).await.unwrap();
    let (router, router_deployment) = (Uuid::new_v4(), Uuid::new_v4());
    sqlx::query("INSERT INTO provider_connections(id,name,provider,credential_ref,enabled) VALUES($1,'Contract router','openrouter','env:UNUSED',true)").bind(router).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO deployments(id,model_id,provider_connection_id,upstream_model,enabled) VALUES($1,$2,$3,'contract-router-upstream',true)").bind(router_deployment).bind(m).bind(router).execute(&pool).await.unwrap();
    // A free price, older than the paid one (the latest version is the paid).
    let free = free_price(&pool, deployment).await;
    let (p1, p2) = (
        price(&pool, deployment).await,
        price(&pool, router_deployment).await,
    );
    let cost_center = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO cost_centers(id,name,code) VALUES($1,'Contract cost center','CC-CONTRACT')",
    )
    .bind(cost_center)
    .execute(&pool)
    .await
    .unwrap();
    let catalog = Uuid::new_v4();
    sqlx::query("INSERT INTO catalogs(id,name) VALUES($1,'Contract catalog')")
        .bind(catalog)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO catalog_models(catalog_id,model_id) VALUES($1,$2)")
        .bind(catalog)
        .bind(m)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO workspace_type_catalogs(kind,catalog_id) VALUES('team',$1)")
        .bind(catalog)
        .execute(&pool)
        .await
        .unwrap();
    let mock = crate::identity::tests::MockProvider::start(None).await;
    let identity = mock.state(pool.clone()).await;
    let engine = crate::inference::Engine::new(
        std::sync::Arc::new(f.s.clone()),
        crate::providers::ProviderRegistry::default(),
        crate::inference::EngineLimits::default(),
    )
    .unwrap();
    let app =
        crate::http::router_with_identity(f.s.clone(), None, engine, identity).layer(Extension(rt));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let _server = Server(tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .unwrap()
    }));
    let mut w = Wire {
        client: reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap(),
        base,
        observed: PositiveFixtures::default(),
        failed: Vec::new(),
    };
    let ws = format!("/api/v1/workspaces/{}", f.team);
    let personal = format!("/api/v1/workspaces/{}", f.personal);
    let project = format!("/api/v1/workspaces/{}", f.project);

    // Layered policy through the real API: platform type defaults (team full,
    // project cleared), then the workspace's own tighter limits.
    let full = |rpm: i64,
                tpm: i64,
                conc: i64,
                jobs: i64,
                storage: i64,
                holder: i64,
                per_ws: i64,
                budgets: Value| {
        json!({"requests_per_minute":rpm,"tokens_per_minute":tpm,"concurrent_requests":conc,"concurrent_jobs":jobs,"storage_bytes":storage,"key_issuances_per_holder":holder,"key_issuances_per_workspace":per_ws,"budgets":budgets})
    };
    w.setup(&admin, "team type policy", "PUT", "/api/v1/platform/workspace-types/team/policy", Some(full(1000, 100_000, 50, 20, 1_099_511_627_776, 50, 100, json!([{"period":"month","amount_microusd":"9007199254740995"},{"period":"lifetime","amount_microusd":"9007199254740995"}])))).await;
    w.setup(&admin, "project type policy", "PUT", "/api/v1/platform/workspace-types/project/policy", Some(json!({"requests_per_minute":null,"tokens_per_minute":null,"concurrent_requests":null,"concurrent_jobs":null,"storage_bytes":null,"key_issuances_per_holder":null,"key_issuances_per_workspace":null,"budgets":[]}))).await;
    w.setup(&owner, "team local policy", "PUT", &format!("{ws}/policy"), Some(full(500, 50_000, 25, 10, 549_755_811_840, 25, 50, json!([{"period":"month","amount_microusd":"9007199254740994"},{"period":"day","amount_microusd":"1000000"}])))).await;

    // Issuance responses alone contain secrets. Read projections forbid them.
    let issued = w
        .json(
            &owner,
            "POST",
            &format!("{ws}/keys"),
            Some(json!({"name":"Owner key","expires_in_days":30})),
        )
        .await;
    let own = id(&issued);
    assert!(issued["policy"].is_null() && issued["model_ids"].is_null());
    let member_issued = w
        .json(
            &member,
            "POST",
            &format!("{ws}/keys"),
            Some(json!({"name":"Member key","expires_in_days":30})),
        )
        .await;
    let member_key = id(&member_issued);
    let inference_token = member_issued["token"].as_str().unwrap().to_owned();
    let private_key = id(&w
        .json(
            &owner,
            "POST",
            &format!("{personal}/keys"),
            Some(json!({"name":"Private key","expires_in_days":30})),
        )
        .await);
    let account = id(&w
        .json(
            &owner,
            "POST",
            &format!("{ws}/service-accounts"),
            Some(json!({"name":"Automation"})),
        )
        .await);
    let service_key = id(&w
        .json(
            &owner,
            "POST",
            &format!("{ws}/keys"),
            Some(json!({"name":"Service key","expires_in_days":30,"service_account_id":account,"tokens_per_minute":5000})),
        )
        .await);
    let log_key = id(&w
        .json(
            &owner,
            "POST",
            &format!("{ws}/keys"),
            Some(json!({"name":"Log key","expires_in_days":30})),
        )
        .await);
    let full_issued = w
        .json(
            &owner,
            "POST",
            &format!("{ws}/keys"),
            Some(json!({"name":"Full key","expires_in_days":30,"model_ids":[m],"requests_per_minute":100,"tokens_per_minute":10000,"concurrent_requests":5,"concurrent_jobs":2,"budgets":[{"period":"month","amount_microusd":"5000000"},{"period":"lifetime","amount_microusd":"4000"}]})),
        )
        .await;
    let full_key = id(&full_issued);
    assert_eq!(full_issued["model_ids"], json!([m]));
    let month_key = id(&w
        .json(
            &owner,
            "POST",
            &format!("{ws}/keys"),
            Some(json!({"name":"Month key","expires_in_days":30,"budgets":[{"period":"month","amount_microusd":"2000000"}]})),
        )
        .await);
    let off_key = id(&w
        .json(
            &owner,
            "POST",
            &format!("{ws}/keys"),
            Some(json!({"name":"Off key","expires_in_days":30})),
        )
        .await);
    let project_key = id(&w
        .json(
            &owner,
            "POST",
            &format!("{project}/keys"),
            Some(json!({"name":"Project key","expires_in_days":30})),
        )
        .await);
    // A throwaway key whose day window is then corrupted (below): its own
    // maintained totals row is the only one touched.
    let corrupt_key = id(&w
        .json(
            &owner,
            "POST",
            &format!("{ws}/keys"),
            Some(json!({"name":"Corrupt key","expires_in_days":30,"budgets":[{"period":"day","amount_microusd":"100"}]})),
        )
        .await);

    // Request logs: every attempt shape the engine, settlement, the realtime
    // close and retention compaction can leave behind.
    let (a, b_root, c, d, e, rt_root, closed_root, g, h, k) = (
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
    );
    let (priced, unpriced) = (Some(p1), None);
    let at = |n: i32, root: Uuid, ago: i64| Ex {
        n,
        session: Some("s-rich"),
        app: Some("Agent"),
        cost_center: Some(cost_center),
        ..Ex::rich(f.team, log_key, deployment, root, ago)
    };
    // A: three attempts of one request: fast failure, unknown-cost failure, success.
    exec(
        &pool,
        Ex {
            state: "failed",
            error: Some("upstream_unavailable"),
            tokens: None,
            reasoning: None,
            finish: Some("error"),
            ttft: None,
            generation: Some(40),
            elapsed: Some(40),
            reported: None,
            billing: None,
            meter: None,
            res: Res::Settled {
                actual: 0,
                price: priced,
            },
            ..at(1, a, 30)
        },
    )
    .await;
    exec(
        &pool,
        Ex {
            state: "failed",
            error: Some("upstream_timeout"),
            tokens: None,
            reasoning: None,
            finish: None,
            ttft: None,
            generation: None,
            elapsed: Some(900),
            app: None,
            reported: None,
            upstream: None,
            billing: Some(BILLING_UNKNOWN),
            meter: Some(METER_UNKNOWN),
            res: Res::Unknown {
                held: Some(5),
                price: unpriced,
                unbounded: false,
            },
            ..at(2, a, 30)
        },
    )
    .await;
    exec(
        &pool,
        Ex {
            res: Res::Settled {
                actual: 7,
                price: priced,
            },
            ..at(3, a, 30)
        },
    )
    .await;
    // B: the member's request that recorded nothing optional and has no reservation.
    exec(
        &pool,
        Ex {
            session: Some("s-plain"),
            app: None,
            cost_center: None,
            ..Ex::sparse(f.team, member_key, deployment, b_root, 25)
        },
    )
    .await;
    // C: still running (pending hold).
    exec(
        &pool,
        Ex {
            state: "started",
            completed: false,
            tokens: None,
            reasoning: None,
            finish: None,
            ttft: None,
            generation: None,
            elapsed: None,
            reported: None,
            billing: None,
            meter: None,
            res: Res::Pending {
                held: 9,
                price: priced,
            },
            ..at(1, c, 20)
        },
    )
    .await;
    // D: a router attempt whose cost is an unbounded unknown.
    exec(
        &pool,
        Ex {
            state: "failed",
            error: Some("upstream_error"),
            provider: "openrouter",
            tokens: Some((3, 0)),
            reasoning: None,
            finish: Some("error"),
            session: None,
            app: None,
            cost_center: None,
            reported: None,
            billing: None,
            meter: None,
            res: Res::Unknown {
                held: None,
                price: Some(p2),
                unbounded: true,
            },
            ..Ex::rich(f.team, log_key, router_deployment, d, 15)
        },
    )
    .await;
    // E: indeterminate, priced, usage incomplete.
    exec(
        &pool,
        Ex {
            state: "indeterminate",
            tokens: Some((1, 0)),
            reasoning: None,
            finish: Some("unknown"),
            session: None,
            app: None,
            cost_center: None,
            billing: None,
            meter: None,
            res: Res::Unknown {
                held: Some(3),
                price: priced,
                unbounded: false,
            },
            ..Ex::rich(f.team, log_key, deployment, e, 12)
        },
    )
    .await;
    // F: a live realtime session: one response settled, one running. The
    // execution is `started` until the session closes (`finish_once`).
    let realtime = exec(
        &pool,
        Ex {
            workload: "realtime",
            streamed: true,
            state: "started",
            completed: false,
            tokens: None,
            reasoning: None,
            finish: None,
            ttft: None,
            generation: None,
            elapsed: None,
            session: None,
            app: None,
            cost_center: None,
            billing: None,
            meter: None,
            res: Res::Pending {
                held: 20,
                price: priced,
            },
            ..Ex::rich(f.team, log_key, deployment, rt_root, 10)
        },
    )
    .await;
    realtime_response(&pool, realtime, 1, "settled").await;
    realtime_response(&pool, realtime, 2, "pending").await;
    // F2: a closed realtime session whose last response never finished: the
    // reservation is unknown (the held figure is the retained window plus the
    // settled response) and no token total is recorded.
    let closed = exec(
        &pool,
        Ex {
            workload: "realtime",
            streamed: true,
            tokens: None,
            reasoning: None,
            finish: None,
            ttft: None,
            generation: None,
            session: None,
            app: None,
            cost_center: None,
            billing: None,
            meter: Some(METER_REALTIME),
            res: Res::Unknown {
                held: Some(10),
                price: priced,
                unbounded: false,
            },
            ..Ex::rich(f.team, log_key, deployment, closed_root, 11)
        },
    )
    .await;
    realtime_response(&pool, closed, 1, "settled").await;
    realtime_response(&pool, closed, 2, "unknown").await;
    // G: compacted: details cleared, every reservation settled. A failure
    // settles only as a free price's pre-processing rejection (known zero,
    // tokens 0, `governance::free_rejection`); a success settles on its usage.
    for (n, error, res, billing, meter, tokens) in [
        (
            1,
            Some("upstream_rejected"),
            Res::Settled {
                actual: 0,
                price: Some(free),
            },
            None,
            None,
            Some((0, 0)),
        ),
        (
            2,
            Some("context_length_exceeded"),
            Res::Settled {
                actual: 0,
                price: Some(free),
            },
            None,
            None,
            Some((0, 0)),
        ),
        (
            3,
            None,
            Res::Settled {
                actual: 7,
                price: priced,
            },
            Some(BILLING_FULL),
            Some(METER_FULL),
            Some((10, 5)),
        ),
    ] {
        exec(
            &pool,
            Ex {
                n,
                redacted: true,
                state: if error.is_some() {
                    "failed"
                } else {
                    "succeeded"
                },
                error,
                res,
                billing,
                meter,
                tokens,
                reasoning: error.map_or(Some(2), |_| None),
                finish: error.map_or(Some("stop"), |_| None),
                ttft: error.map_or(Some(100), |_| None),
                generation: error.map_or(Some(600), |_| None),
                reported: error.map_or(Some("served-upstream-v2"), |_| None),
                ..at(n, g, 60)
            },
        )
        .await;
    }
    // H: a compacted realtime session (compaction needs a settled reservation,
    // and a realtime reservation settles only when every response did).
    let compacted = exec(
        &pool,
        Ex {
            workload: "realtime",
            redacted: true,
            provider: "openrouter",
            tokens: Some((10, 22)),
            reasoning: None,
            finish: None,
            ttft: None,
            generation: None,
            billing: None,
            meter: Some(METER_REALTIME),
            res: Res::Settled {
                actual: 8,
                price: Some(p2),
            },
            ..Ex::rich(f.team, log_key, router_deployment, h, 55)
        },
    )
    .await;
    realtime_response(&pool, compacted, 1, "settled").await;
    realtime_response(&pool, compacted, 2, "settled_unreported").await;
    let envelope_root = Uuid::new_v4();
    // H2: a compacted gateway-run batch envelope. Only a native batch pins a
    // price (`governance::batch`), so the envelope's reservation has none and
    // `close_batch` settles it at a known zero with zero tokens.
    exec(
        &pool,
        Ex {
            workload: "batches",
            streamed: false,
            redacted: true,
            tokens: Some((0, 0)),
            reasoning: None,
            ttft: None,
            generation: None,
            upstream: None,
            reported: None,
            billing: None,
            meter: None,
            res: Res::Settled {
                actual: 0,
                price: None,
            },
            ..Ex::rich(f.team, log_key, deployment, envelope_root, 50)
        },
    )
    .await;
    // K: one settled request of the key with a policy, in its own session.
    exec(
        &pool,
        Ex {
            session: Some("s-known"),
            res: Res::Settled {
                actual: 7,
                price: priced,
            },
            ..Ex::rich(f.team, full_key, deployment, k, 8)
        },
    )
    .await;
    // One settled request of the throwaway key creates its day totals row (the
    // trigger). It is corrupted only while its reports are read (below).
    exec(
        &pool,
        Ex {
            res: Res::Settled {
                actual: 7,
                price: priced,
            },
            ..Ex::rich(f.team, corrupt_key, deployment, Uuid::new_v4(), 9)
        },
    )
    .await;
    let private_root = Uuid::new_v4();
    exec(
        &pool,
        Ex {
            cost_center: Some(cost_center),
            res: Res::Settled {
                actual: 7,
                price: priced,
            },
            ..Ex::rich(f.personal, private_key, deployment, private_root, 7)
        },
    )
    .await;
    let member_root = b_root;
    // Async jobs: a finished gateway batch, a running one waiting on capacity,
    // native batches (priced at the batch tier and at standard), a legacy
    // passthrough batch and two videos.
    let uploadpath = format!("{ws}/files/upload");
    let mut stored: Vec<(String, Uuid)> = Vec::new();
    for (typed, expires, purpose) in [
        (true, true, "user_data"),
        (false, false, "user_data"),
        (true, false, "user_data"),
        (true, false, "user_data"),
        (true, false, "user_data"),
        (true, false, "user_data"),
        (true, false, "user_data"),
    ] {
        let mut fields = vec![("purpose", purpose)];
        if expires {
            fields.push(("expires_after[anchor]", "created_at"));
            fields.push(("expires_after[seconds]", "3600"));
        }
        let r = upload(
            &mut w,
            &owner,
            &uploadpath,
            &fields,
            Some(b"opaque contents"),
            typed,
        )
        .await;
        assert_eq!(r.status, StatusCode::OK);
        let file: Value = serde_json::from_slice(&r.bytes).unwrap();
        let public = file["id"].as_str().unwrap().to_owned();
        let parsed = crate::filestore::files::parse_public_id(&public).unwrap();
        stored.push((public, parsed));
    }
    let (fileid, input_a, input_b, input_c) =
        (stored[0].0.clone(), stored[2].1, stored[3].1, stored[4].1);
    let (output_file, error_file) = (stored[5].1, stored[6].1);
    let root_of = |ago: i64, workload: &'static str, root: Uuid| Ex {
        workload,
        streamed: false,
        res: Res::Settled {
            actual: 3,
            price: priced,
        },
        ..Ex::rich(f.team, log_key, deployment, root, ago)
    };
    let (jb0, jb1, jb2, jb3, jb4, jv1, jv2) = (
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
    );
    for (ago, workload, root) in [
        (5, "batches", jb0),
        (4, "batches", jb1),
        (3, "batches", jb2),
        (2, "batches", jb3),
        (2, "batches", jb4),
        (6, "videos", jv1),
        (9, "videos", jv2),
    ] {
        exec(&pool, root_of(ago, workload, root)).await;
    }
    let owner_id = Some(f.owner.user_id);
    // J0: the cancellable gateway batch (queued).
    let j0 = job(
        &pool,
        "batch",
        f.team,
        log_key,
        deployment,
        jb0,
        Some("gateway"),
        Some(input_a),
        owner_id,
        None,
        Some((1, Some(0), Some(0))),
    )
    .await;
    // J1: a finished gateway batch with results and a failed line.
    let j1 = job(
        &pool,
        "batch",
        f.team,
        log_key,
        deployment,
        jb1,
        Some("gateway"),
        Some(input_b),
        owner_id,
        None,
        Some((3, Some(2), Some(1))),
    )
    .await;
    sqlx::query("UPDATE async_jobs SET state='in_progress',in_progress_at=now(),last_progress_at=now() WHERE id=$1").bind(j1).execute(&pool).await.unwrap();
    sqlx::query("UPDATE async_jobs SET state='completed',finalizing_at=now(),completed_at=now(),settled_at=now(),output_file_id=$2,error_file_id=$3 WHERE id=$1").bind(j1).bind(output_file).bind(error_file).execute(&pool).await.unwrap();
    line(&pool, f.team, j1, 0, "succeeded", None, None).await;
    line(&pool, f.team, j1, 1, "succeeded", None, None).await;
    line(
        &pool,
        f.team,
        j1,
        2,
        "failed",
        Some("invalid_request"),
        None,
    )
    .await;
    // J2: a running gateway batch whose lines wait on capacity.
    let j2 = job(
        &pool,
        "batch",
        f.team,
        log_key,
        deployment,
        jb2,
        Some("gateway"),
        Some(input_c),
        owner_id,
        None,
        Some((4, Some(0), Some(0))),
    )
    .await;
    sqlx::query("UPDATE async_jobs SET state='in_progress',in_progress_at=now(),last_progress_at=now() WHERE id=$1").bind(j2).execute(&pool).await.unwrap();
    line(&pool, f.team, j2, 0, "running", None, Some(deployment)).await;
    line(
        &pool,
        f.team,
        j2,
        1,
        "running",
        None,
        Some(router_deployment),
    )
    .await;
    sqlx::query("INSERT INTO batch_route_waits(job_id,workspace_id,deployment_id,waiting_lines,reason,ready) VALUES($1,$2,$3,2,'workers',false),($1,$2,$4,1,NULL,true)").bind(j2).bind(f.team).bind(deployment).bind(router_deployment).execute(&pool).await.unwrap();
    // J3: a native batch priced at the batch tier, cancelled upstream.
    let j3 = job(
        &pool,
        "batch",
        f.team,
        log_key,
        deployment,
        jb3,
        Some("native"),
        Some(input_a),
        owner_id,
        Some("batch"),
        Some((5, Some(1), Some(1))),
    )
    .await;
    sqlx::query("UPDATE async_jobs SET state='in_progress',upstream_status='validating',in_progress_at=now(),last_progress_at=now() WHERE id=$1").bind(j3).execute(&pool).await.unwrap();
    sqlx::query("UPDATE async_jobs SET state='cancelled',upstream_status='cancelled',cancel_requested_at=now(),completed_at=now(),settled_at=now(),error_code='batch_cancelled' WHERE id=$1").bind(j3).execute(&pool).await.unwrap();
    // J4: a legacy passthrough batch (no gateway copy) whose counts are still unknown.
    let j4 = job(
        &pool,
        "batch",
        f.team,
        log_key,
        deployment,
        jb4,
        None,
        None,
        None,
        Some("standard"),
        Some((5, None, None)),
    )
    .await;
    let j5root = Uuid::new_v4();
    exec(&pool, root_of(2, "batches", j5root)).await;
    let j5 = job(
        &pool, "batch", f.team, log_key, deployment, j5root, None, None, None, None, None,
    )
    .await;
    // Videos: one running with progress, one finished, cancelled and deleted.
    let v1 = job(
        &pool, "video", f.team, log_key, deployment, jv1, None, None, None, None, None,
    )
    .await;
    sqlx::query("UPDATE async_jobs SET state='in_progress',upstream_status='processing',progress=40,expires_at=now()+interval '1 hour' WHERE id=$1").bind(v1).execute(&pool).await.unwrap();
    let v2 = job(
        &pool, "video", f.team, log_key, deployment, jv2, None, None, None, None, None,
    )
    .await;
    sqlx::query("UPDATE async_jobs SET state='failed',upstream_status='failed',cancel_requested_at=now(),completed_at=now(),settled_at=now(),deleted_at=now(),error_code='upstream_failed' WHERE id=$1").bind(v2).execute(&pool).await.unwrap();

    // The owner's personal workspace holds the same shapes: rich (above, with a
    // cost center), sparse, running, and an async job of every kind.
    let (pb, pc, jp1r, jp2r, jp3r, jp4r) = (
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
    );
    exec(
        &pool,
        Ex::sparse(f.personal, private_key, deployment, pb, 6),
    )
    .await;
    exec(
        &pool,
        Ex {
            state: "started",
            completed: false,
            tokens: None,
            reasoning: None,
            finish: None,
            ttft: None,
            generation: None,
            elapsed: None,
            reported: None,
            billing: None,
            meter: None,
            res: Res::Pending {
                held: 9,
                price: priced,
            },
            ..Ex::rich(f.personal, private_key, deployment, pc, 5)
        },
    )
    .await;
    for (ago, workload, root) in [
        (4, "batches", jp1r),
        (3, "batches", jp2r),
        (2, "videos", jp3r),
        (1, "videos", jp4r),
    ] {
        exec(
            &pool,
            Ex {
                workload,
                streamed: false,
                res: Res::Settled {
                    actual: 3,
                    price: priced,
                },
                ..Ex::rich(f.personal, private_key, deployment, root, ago)
            },
        )
        .await;
    }
    let jp1 = job(
        &pool,
        "batch",
        f.personal,
        private_key,
        deployment,
        jp1r,
        Some("gateway"),
        Some(input_a),
        owner_id,
        None,
        Some((3, Some(2), Some(1))),
    )
    .await;
    sqlx::query("UPDATE async_jobs SET state='in_progress',in_progress_at=now(),last_progress_at=now() WHERE id=$1").bind(jp1).execute(&pool).await.unwrap();
    sqlx::query("UPDATE async_jobs SET state='completed',finalizing_at=now(),completed_at=now(),settled_at=now() WHERE id=$1").bind(jp1).execute(&pool).await.unwrap();
    let jp2 = job(
        &pool,
        "batch",
        f.personal,
        private_key,
        deployment,
        jp2r,
        None,
        None,
        None,
        Some("standard"),
        Some((5, None, None)),
    )
    .await;
    sqlx::query(
        "UPDATE async_jobs SET state='in_progress',upstream_status='validating' WHERE id=$1",
    )
    .bind(jp2)
    .execute(&pool)
    .await
    .unwrap();
    let jp3 = job(
        &pool,
        "video",
        f.personal,
        private_key,
        deployment,
        jp3r,
        None,
        None,
        None,
        None,
        None,
    )
    .await;
    sqlx::query("UPDATE async_jobs SET state='in_progress',upstream_status='processing',progress=40,expires_at=now()+interval '1 hour' WHERE id=$1").bind(jp3).execute(&pool).await.unwrap();
    let jp4 = job(
        &pool,
        "video",
        f.personal,
        private_key,
        deployment,
        jp4r,
        None,
        None,
        None,
        None,
        None,
    )
    .await;
    sqlx::query("UPDATE async_jobs SET state='failed',upstream_status='failed',cancel_requested_at=now(),completed_at=now(),settled_at=now(),deleted_at=now(),error_code='upstream_failed' WHERE id=$1").bind(jp4).execute(&pool).await.unwrap();

    // Audit rows the system itself writes (no person acts): the producers'
    // own statements (lifecycle.rs, bootstrap.rs).
    sqlx::query("INSERT INTO audit_events(id,actor_user_id,action,resource_type,resource_id) VALUES($1,NULL,'user.cleaned','user',$2)").bind(Uuid::new_v4()).bind(f.outsider.user_id).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO audit_events(id,workspace_id,action,resource_type,resource_id,metadata) VALUES($1,$2,'access_request.expired','access_request',$3,'{\"kind\":\"workspace_membership\",\"outcome\":\"expired\"}'::jsonb)").bind(Uuid::new_v4()).bind(f.team).bind(Uuid::new_v4()).execute(&pool).await.unwrap();
    // A non-member's workspace change: the actor is hidden from workspace readers.
    sqlx::query("INSERT INTO audit_events(id,actor_user_id,workspace_id,action,resource_type) VALUES($1,$2,$3,'workspace.updated','workspace')").bind(Uuid::new_v4()).bind(f.outsider.user_id).bind(f.team).execute(&pool).await.unwrap();

    sqlx::query(
        "UPDATE api_keys SET expires_at=NULL,created_at=now()-interval '200 days' WHERE id=$1",
    )
    .bind(member_key)
    .execute(&pool)
    .await
    .unwrap();
    let keypath = format!("{ws}/keys/{own}");
    w.json(&owner,"PUT",&format!("{keypath}/policy"),Some(json!({"requests_per_minute":null,"tokens_per_minute":null,"concurrent_requests":null,"budgets":[{"period":"month","amount_microusd":"9007199254740993"}]}))).await;

    // Keys: lists, details and every projection, before any lifecycle change.
    for suffix in ["", "/access", "/policy", "/stats"] {
        let v = w
            .json(&owner, "GET", &format!("{keypath}{suffix}"), None)
            .await;
        assert!(v.get("token").is_none() && v.get("secret_hash").is_none());
    }
    for key in [full_key, month_key, service_key, log_key, off_key] {
        for suffix in ["", "/policy", "/stats"] {
            w.json(&owner, "GET", &format!("{ws}/keys/{key}{suffix}"), None)
                .await;
        }
        w.json(
            &owner,
            "GET",
            &format!("{ws}/keys/{key}/access?model_id={m}"),
            None,
        )
        .await;
    }
    // The throwaway key's day totals row is corrupted exactly as
    // `governance/tests.rs` corrupts a workspace row, so its policy and stats
    // reports show unavailable windows (`read_budget_report` returns None); the
    // row is restored right after, because the key list and detail report the
    // raw figure, which a corrupt row cannot honestly satisfy.
    let (settled, held): (String, String) = sqlx::query_as("SELECT settled_microusd::text,held_microusd::text FROM budget_totals WHERE scope_kind='key' AND period='day' AND scope_id=(SELECT governance_key_id FROM api_keys WHERE id=$1)")
        .bind(corrupt_key).fetch_one(&pool).await.unwrap();
    let set_totals = "UPDATE budget_totals SET settled_microusd=$2::numeric,held_microusd=$3::numeric WHERE scope_kind='key' AND period='day' AND scope_id=(SELECT governance_key_id FROM api_keys WHERE id=$1)";
    let corrupted = sqlx::query(set_totals)
        .bind(corrupt_key)
        .bind("-1")
        .bind("0")
        .execute(&pool)
        .await
        .unwrap()
        .rows_affected();
    assert_eq!(corrupted, 1, "the throwaway key has one day totals row");
    for suffix in ["/policy", "/stats"] {
        w.json(
            &owner,
            "GET",
            &format!("{ws}/keys/{corrupt_key}{suffix}"),
            None,
        )
        .await;
    }
    sqlx::query(set_totals)
        .bind(corrupt_key)
        .bind(&settled)
        .bind(&held)
        .execute(&pool)
        .await
        .unwrap();
    for suffix in ["", "/policy", "/stats", "/access"] {
        w.json(
            &owner,
            "GET",
            &format!("{project}/keys/{project_key}{suffix}"),
            None,
        )
        .await;
    }
    w.json(&owner, "GET", &format!("{ws}/keys?limit=3"), None)
        .await;
    w.json(&owner, "GET", &format!("{ws}/keys?q=key"), None)
        .await;
    w.json(&owner, "GET", &format!("{ws}/keys"), None).await;
    let list = w.json(&member, "GET", &format!("{ws}/keys"), None).await;
    assert_eq!(list["data"].as_array().unwrap().len(), 1);
    assert_eq!(list["data"][0]["id"], json!(member_key));
    let policy = w
        .json(
            &member,
            "GET",
            &format!("{ws}/keys/{member_key}/policy"),
            None,
        )
        .await;
    assert!(
        policy["budgets"]
            .as_array()
            .unwrap()
            .iter()
            .any(|b| b["usage_visible"] == false && b["used_microusd"].is_null())
    );
    w.json(
        &member,
        "GET",
        &format!("{ws}/keys/{member_key}/stats"),
        None,
    )
    .await;
    // A platform override changes the policy and access views of the same keys.
    for (rpm, tpm, conc, jobs, storage, holder, per_ws, budgets) in [
        (
            json!(900),
            json!(null),
            json!(40),
            json!(null),
            json!(1_099_511_627_776_i64),
            json!(null),
            json!(80),
            json!([{"period":"month","amount_microusd":"9007199254740994"}]),
        ),
        (
            json!(null),
            json!(90_000),
            json!(null),
            json!(15),
            json!(null),
            json!(40),
            json!(null),
            json!([]),
        ),
    ] {
        w.setup(&admin, "workspace override", "PUT", &format!("/api/v1/platform/workspaces/{}/policy", f.team), Some(json!({"requests_per_minute":rpm,"tokens_per_minute":tpm,"concurrent_requests":conc,"concurrent_jobs":jobs,"storage_bytes":storage,"key_issuances_per_holder":holder,"key_issuances_per_workspace":per_ws,"budgets":budgets}))).await;
        for key in [full_key, off_key] {
            w.json(&owner, "GET", &format!("{ws}/keys/{key}/policy"), None)
                .await;
            w.json(&owner, "GET", &format!("{ws}/keys/{key}/access"), None)
                .await;
        }
    }
    // ... and so does a workspace catalog override.
    let second_catalog = Uuid::new_v4();
    sqlx::query("INSERT INTO catalogs(id,name) VALUES($1,'Override catalog')")
        .bind(second_catalog)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO workspace_catalog_overrides(workspace_id) VALUES($1)")
        .bind(f.team)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO workspace_catalog_override_items(workspace_id,catalog_id) VALUES($1,$2)",
    )
    .bind(f.team)
    .bind(second_catalog)
    .execute(&pool)
    .await
    .unwrap();
    w.json(&owner, "GET", &format!("{ws}/keys/{full_key}/access"), None)
        .await;
    sqlx::query("DELETE FROM workspace_catalog_override_items WHERE workspace_id=$1")
        .bind(f.team)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM workspace_catalog_overrides WHERE workspace_id=$1")
        .bind(f.team)
        .execute(&pool)
        .await
        .unwrap();

    // Lifecycle: disable, rotate, revoke; service accounts.
    w.json(
        &owner,
        "PATCH",
        &format!("{ws}/keys/{off_key}"),
        Some(json!({"disabled":true})),
    )
    .await;
    assert_eq!(
        w.json(&owner, "GET", &format!("{ws}/keys/{off_key}"), None)
            .await["status"],
        "disabled"
    );
    // The member's key never expires (a finding) and is read by its holder.
    w.json(&member, "GET", &format!("{ws}/keys/{member_key}"), None)
        .await;
    w.json(&owner, "PATCH", &keypath, Some(json!({"disabled":true})))
        .await;
    w.status(
        &owner,
        "POST",
        &format!("{keypath}/rotate"),
        Some(json!({"expires_in_days":30})),
        StatusCode::CONFLICT,
    )
    .await;
    w.json(&owner, "PATCH", &keypath, Some(json!({"disabled":false})))
        .await;
    let replacement = w
        .json(
            &owner,
            "POST",
            &format!("{keypath}/rotate"),
            Some(json!({"expires_in_days":30})),
        )
        .await;
    assert!(replacement.get("token").is_some());
    let rotated = id(&replacement);
    let old = w.json(&owner, "GET", &keypath, None).await;
    assert_eq!(old["status"], "revoked");
    let new = w
        .json(&owner, "GET", &format!("{ws}/keys/{rotated}"), None)
        .await;
    assert_eq!(old["lineage_id"], new["lineage_id"]);
    w.status(
        &owner,
        "PATCH",
        &keypath,
        Some(json!({"disabled":false})),
        StatusCode::CONFLICT,
    )
    .await;
    w.json(&owner, "DELETE", &format!("{ws}/keys/{rotated}"), None)
        .await;
    w.json(&owner, "GET", &format!("{ws}/service-accounts"), None)
        .await;
    for disabled in [true, false] {
        w.json(
            &owner,
            "PATCH",
            &format!("{ws}/service-accounts/{account}"),
            Some(json!({"disabled":disabled})),
        )
        .await;
        w.json(&owner, "GET", &format!("{ws}/service-accounts"), None)
            .await;
    }
    assert_eq!(
        w.json(&owner, "GET", &format!("{ws}/keys/{service_key}"), None)
            .await["status"],
        "revoked"
    );
    // A fresh service key (the first was revoked with its account) is the
    // service-account holder in the key-safety views.
    w.json(
        &owner,
        "POST",
        &format!("{ws}/keys"),
        Some(json!({"name":"Service key two","expires_in_days":30,"service_account_id":account})),
    )
    .await;
    let v = w
        .json(&owner, "GET", &format!("{ws}/key-issuance"), None)
        .await;
    assert!(v.get("workspace").is_some());
    assert!(
        w.json(&member, "GET", &format!("{ws}/key-issuance"), None)
            .await
            .get("workspace")
            .is_none()
    );
    // Key safety: every finding shape (no value, days, models).
    let safety_ws = w
        .json(&owner, "GET", &format!("{ws}/key-safety"), None)
        .await;
    assert!(!safety_ws["data"].as_array().unwrap().is_empty());
    w.json(&member, "GET", &format!("{ws}/key-safety"), None)
        .await;
    w.json(&owner, "GET", &format!("{project}/key-safety"), None)
        .await;
    let safety = w
        .json(&auditor, "GET", "/api/v1/platform/key-safety", None)
        .await;
    assert!(
        safety["data"]
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r["key"].get("name").is_none() && r["key"].get("issued_to_user_id").is_none())
    );

    // Request logs of every form, with paging.
    for suffix in ["/executions", "/logs/metrics"] {
        w.json(&owner, "GET", &format!("{ws}{suffix}"), None).await;
    }
    for resource in ["requests", "generations"] {
        let first = w
            .json(&owner, "GET", &format!("{ws}/{resource}?limit=2"), None)
            .await;
        let cursor = first["next_cursor"].as_str().unwrap().to_owned();
        w.json(
            &owner,
            "GET",
            &format!("{ws}/{resource}?limit=2&cursor={cursor}"),
            None,
        )
        .await;
        w.json(&owner, "GET", &format!("{ws}/{resource}"), None)
            .await;
        let first = w
            .json(
                &auditor,
                "GET",
                &format!("/api/v1/platform/logs/{resource}?limit=2"),
                None,
            )
            .await;
        let cursor = first["next_cursor"].as_str().unwrap().to_owned();
        w.json(
            &auditor,
            "GET",
            &format!("/api/v1/platform/logs/{resource}?limit=2&cursor={cursor}"),
            None,
        )
        .await;
    }
    let own_member_logs = w
        .json(&member, "GET", &format!("{ws}/requests"), None)
        .await;
    assert_eq!(own_member_logs["data"].as_array().unwrap().len(), 1);
    assert_eq!(
        own_member_logs["data"][0]["root_request_id"],
        json!(member_root)
    );
    w.json(&owner, "GET", &format!("{personal}/requests"), None)
        .await;
    // Platform session lists (and one page boundary) and metrics.
    let sessions = w
        .json(
            &auditor,
            "GET",
            "/api/v1/platform/logs/sessions?limit=1",
            None,
        )
        .await;
    let cursor = sessions["next_cursor"].as_str().unwrap().to_owned();
    w.json(
        &auditor,
        "GET",
        &format!("/api/v1/platform/logs/sessions?limit=1&cursor={cursor}"),
        None,
    )
    .await;
    for suffix in ["generations", "metrics", "requests", "sessions"] {
        let v = w
            .json_any(
                &auditor,
                "GET",
                &format!("/api/v1/platform/logs/{suffix}"),
                None,
            )
            .await;
        assert!(!v.to_string().contains(&private_root.to_string()));
    }
    for session in ["s-rich", "s-plain", "s-known"] {
        w.json(
            &auditor,
            "GET",
            &format!("/api/v1/platform/logs/sessions/{}/{session}", f.team),
            None,
        )
        .await;
    }
    fresh_waits(&pool).await;
    // Every request's detail, to its owner and to the platform reader.
    for r in [
        a,
        b_root,
        c,
        d,
        e,
        rt_root,
        closed_root,
        envelope_root,
        g,
        h,
        k,
        jb0,
        jb1,
        jb2,
        jb3,
        jb4,
        jv1,
        jv2,
        j5root,
    ] {
        let v = w
            .json(&owner, "GET", &format!("{ws}/requests/{r}"), None)
            .await;
        assert!(v.get("prompt").is_none() && v.get("response").is_none());
        w.json(
            &auditor,
            "GET",
            &format!("/api/v1/platform/logs/requests/{r}"),
            None,
        )
        .await;
    }
    let first = w
        .json(&owner, "GET", &format!("{ws}/requests/{a}"), None)
        .await;
    assert!(
        first["cost_microusd"].is_null(),
        "an unknown attempt cost is unknown, not zero"
    );
    assert_eq!(first["attempts"][1]["unresolved_reason"], "unpriced");
    let missing_reservation = w
        .json(&owner, "GET", &format!("{ws}/requests/{b_root}"), None)
        .await;
    assert_eq!(
        missing_reservation["attempts"][0]["unresolved_reason"],
        "missing_reservation"
    );
    assert!(missing_reservation["cost_microusd"].is_null());
    let compacted_root = w
        .json(&owner, "GET", &format!("{ws}/requests/{g}"), None)
        .await;
    assert!(compacted_root["attempts"][0]["details_redacted_at"].is_string());
    assert_eq!(compacted_root["cost_microusd"], "7");
    let private = w
        .json(
            &owner,
            "GET",
            &format!("{personal}/requests/{private_root}"),
            None,
        )
        .await;
    assert!(private["trace_id"].is_null());
    for r in [pb, pc, jp1r, jp2r, jp3r, jp4r] {
        let v = w
            .json(&owner, "GET", &format!("{personal}/requests/{r}"), None)
            .await;
        assert!(v["trace_id"].is_null());
    }
    w.status(
        &admin,
        "GET",
        &format!("{personal}/requests/{private_root}"),
        None,
        StatusCode::NOT_FOUND,
    )
    .await;
    w.status(
        &member,
        "GET",
        &format!("{ws}/requests/{a}"),
        None,
        StatusCode::NOT_FOUND,
    )
    .await;
    let jobs = w.json(&owner, "GET", &format!("{ws}/requests"), None).await;
    assert!(
        jobs["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["job"]["kind"] == "batch")
    );
    let job_request = w
        .json(&owner, "GET", &format!("{ws}/requests/{jb1}"), None)
        .await;
    assert_eq!(job_request["job"]["kind"], "batch");
    assert_eq!(job_request["job"]["request_counts"]["total"], 3);

    // Multipart download and opaque binary; raw wire capture precedes
    // decoding. Visibility widens independently of delete permission.
    w.json(&owner, "GET", &format!("{ws}/files"), None).await;
    w.json_any(&owner, "GET", &format!("{project}/files"), None)
        .await;
    let download = format!("{ws}/files/{fileid}/content");
    w.status(&member, "GET", &download, None, StatusCode::NOT_FOUND)
        .await;
    let r = w.send(&owner, "GET", &download, None).await;
    assert_eq!(r.bytes.as_ref(), b"opaque contents");
    sqlx::query("UPDATE workspaces SET object_visibility='workspace' WHERE id=$1")
        .bind(f.team)
        .execute(&pool)
        .await
        .unwrap();
    let files = w.json(&member, "GET", &format!("{ws}/files"), None).await;
    assert!(files["storage"]["used_bytes"].is_null());
    w.status(
        &member,
        "DELETE",
        &format!("{ws}/files/{fileid}"),
        None,
        StatusCode::FORBIDDEN,
    )
    .await;

    // Batches: lists and details of every kind; the gateway batch is cancelled.
    let batchpath = format!("{ws}/batches/batch_{}", j0.simple());
    let batches = w.json(&member, "GET", &format!("{ws}/batches"), None).await;
    assert_eq!(batches["scope"], "workspace");
    w.status(
        &member,
        "POST",
        &format!("{batchpath}/cancel"),
        None,
        StatusCode::FORBIDDEN,
    )
    .await;
    sqlx::query("UPDATE workspaces SET object_visibility='member' WHERE id=$1")
        .bind(f.team)
        .execute(&pool)
        .await
        .unwrap();
    w.status(&member, "GET", &batchpath, None, StatusCode::NOT_FOUND)
        .await;
    fresh_waits(&pool).await;
    w.json(&owner, "GET", &format!("{ws}/batches"), None).await;
    for j in [j0, j1, j2, j3, j4, j5] {
        w.json(
            &owner,
            "GET",
            &format!("{ws}/batches/batch_{}", j.simple()),
            None,
        )
        .await;
    }
    w.json(&auditor, "GET", "/api/v1/platform/batches", None)
        .await;
    w.json(&owner, "POST", &format!("{batchpath}/cancel"), None)
        .await;
    w.json(&owner, "DELETE", &format!("{ws}/files/{fileid}"), None)
        .await;

    // Legal holds on a person and on a workspace; one released.
    let hold = id(&w
        .json(
            &admin,
            "POST",
            "/api/v1/platform/legal-holds",
            Some(json!({"workspace_id":f.team,"reference":"CONTRACT-1"})),
        )
        .await);
    w.json(
        &admin,
        "POST",
        "/api/v1/platform/legal-holds",
        Some(json!({"user_id":f.member.user_id,"reference":"CONTRACT-2"})),
    )
    .await;
    w.json(&auditor, "GET", "/api/v1/platform/legal-holds", None)
        .await;
    w.status(
        &auditor,
        "POST",
        &format!("/api/v1/platform/legal-holds/{hold}/release"),
        None,
        StatusCode::FORBIDDEN,
    )
    .await;
    w.json(
        &admin,
        "POST",
        &format!("/api/v1/platform/legal-holds/{hold}/release"),
        None,
    )
    .await;
    w.json(
        &auditor,
        "GET",
        "/api/v1/platform/legal-holds?state=all",
        None,
    )
    .await;
    w.status(
        &admin,
        "POST",
        &format!("/api/v1/platform/legal-holds/{hold}/release"),
        None,
        StatusCode::CONFLICT,
    )
    .await;
    w.status(
        &admin,
        "POST",
        "/api/v1/platform/legal-holds",
        Some(json!({"workspace_id":f.personal,"reference":"CONTRACT-PRIVATE"})),
        StatusCode::BAD_REQUEST,
    )
    .await;

    // Admin tokens: an allowlisted token that is used, a revoked one, and one
    // whose owner's account was later cleaned up.
    let token = w
        .json(
            &admin,
            "POST",
            "/api/v1/platform/admin-tokens",
            Some(json!({"name":"Contract management","scopes":["platform:read"],"expires_in_days":30,"ip_allowlist":["127.0.0.0/8"]})),
        )
        .await;
    let tokenid = id(&token);
    let revoked_token = id(&w
        .json(
            &admin,
            "POST",
            "/api/v1/platform/admin-tokens",
            Some(json!({"name":"Revoked","scopes":["platform:read"],"expires_in_days":30})),
        )
        .await);
    let second = user(&pool, "admin-two", "admin").await;
    let second_browser = browser(&pool, &second).await;
    w.json(
        &second_browser,
        "POST",
        "/api/v1/platform/admin-tokens",
        Some(json!({"name":"Departed owner","scopes":["platform:read"],"expires_in_days":30})),
    )
    .await;
    // A person's token being used records when and from where.
    let used = capture_http(
        "GET",
        "/api/v1/platform/key-safety",
        w.client
            .get(format!("{}/api/v1/platform/key-safety", w.base))
            .bearer_auth(token["token"].as_str().unwrap())
            .send()
            .await
            .expect("loopback request failed"),
    )
    .await;
    assert_eq!(used.status, StatusCode::OK);
    w.json(&admin, "GET", "/api/v1/platform/admin-tokens", None)
        .await;
    sqlx::query("UPDATE users SET display_name='Platform Admin' WHERE id=$1")
        .bind(f.admin.user_id)
        .execute(&pool)
        .await
        .unwrap();
    // The cleanup statement of lifecycle.rs: the owner's identity is removed.
    sqlx::query("UPDATE users SET email=NULL,display_name=NULL,oidc_link_allowed=false,cleaned_at=now(),disabled_at=coalesce(disabled_at,now()) WHERE id=$1 AND cleaned_at IS NULL").bind(second.user_id).execute(&pool).await.unwrap();
    w.json(
        &admin,
        "DELETE",
        &format!("/api/v1/platform/admin-tokens/{revoked_token}"),
        None,
    )
    .await;
    let inventory = w
        .json(&admin, "GET", "/api/v1/platform/admin-tokens", None)
        .await;
    assert!(
        inventory["data"]
            .as_array()
            .unwrap()
            .iter()
            .all(|t| t.get("token").is_none() && t.get("secret_hash").is_none())
    );
    w.status(
        &auditor,
        "GET",
        "/api/v1/platform/admin-tokens",
        None,
        StatusCode::FORBIDDEN,
    )
    .await;
    // Credential families are exclusive, and inference keys never authorize management.
    for token in [inference_token.as_str(), token["token"].as_str().unwrap()] {
        let path = "/api/v1/platform/admin-tokens";
        let r = capture_http(
            "GET",
            path,
            w.client
                .get(format!("{}{path}", w.base))
                .bearer_auth(token)
                .send()
                .await
                .expect("loopback request failed"),
        )
        .await;
        assert!(matches!(
            r.status,
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
        ));
    }
    w.json(
        &admin,
        "DELETE",
        &format!("/api/v1/platform/admin-tokens/{tokenid}"),
        None,
    )
    .await;
    // Audit: the actions above, by people and by the system.
    w.json(&owner, "GET", &format!("{ws}/audit"), None).await;
    w.json(&owner, "GET", &format!("{ws}/audit?limit=1&offset=1"), None)
        .await;
    w.json(&auditor, "GET", "/api/v1/platform/audit", None)
        .await;

    // HTTP boundary vectors of every read that takes query parameters ...
    // The window (1 to 93 days, ending by tomorrow) holds whichever bound is omitted.
    let today = chrono::Utc::now().date_naive();
    let (start_date, end_date) = (
        (today - chrono::Duration::days(1)).to_string(),
        (today + chrono::Duration::days(1)).to_string(),
    );
    let (log_key_text, root_prefix) = (log_key.to_string(), a.to_string()[..8].to_owned());
    let logs: [(&str, &str); 12] = [
        ("start_date", &start_date),
        ("end_date", &end_date),
        ("model", "contract-log"),
        ("key_id", "00000000-0000-0000-0000-000000000001"),
        ("status", "succeeded"),
        ("q", "0000"),
        ("finish_reason", "stop"),
        ("streamed", "true"),
        ("session_id", "s-rich"),
        ("workload", "generation"),
        ("cursor", "1_00000000-0000-0000-0000-000000000001"),
        ("limit", "5"),
    ];
    let no_page: Vec<(&str, &str)> = logs[..10].to_vec();
    let team = f.team.to_string();
    let mut platform_logs = logs.to_vec();
    platform_logs.push(("workspace_id", &team));
    let mut platform_filters = no_page.clone();
    platform_filters.push(("workspace_id", &team));
    let session_cursor = format!("1_{}_{}", team, "73");
    let mut platform_sessions = platform_logs.clone();
    for (n, v) in platform_sessions.iter_mut() {
        if *n == "cursor" {
            *v = &session_cursor;
        }
    }
    let mut platform_session = platform_filters.clone();
    platform_session.retain(|(n, _)| *n != "session_id");
    // A session read answers 200 only when the filters match one of its requests.
    for (n, v) in platform_session.iter_mut() {
        match *n {
            "key_id" => *v = &log_key_text,
            "q" => *v = &root_prefix,
            "status" => *v = "succeeded",
            "finish_reason" => *v = "stop",
            _ => {}
        }
    }
    let workspace_logs: Vec<(&str, &str)> = logs.to_vec();
    query_ops(
        &mut w,
        &auditor,
        "/api/v1/platform/logs/generations",
        "/api/v1/platform/logs/generations",
        &platform_logs,
    )
    .await;
    query_ops(
        &mut w,
        &auditor,
        "/api/v1/platform/logs/requests",
        "/api/v1/platform/logs/requests",
        &platform_logs,
    )
    .await;
    query_ops(
        &mut w,
        &auditor,
        "/api/v1/platform/logs/sessions",
        "/api/v1/platform/logs/sessions",
        &platform_sessions,
    )
    .await;
    query_ops(
        &mut w,
        &auditor,
        "/api/v1/platform/logs/metrics",
        "/api/v1/platform/logs/metrics",
        &platform_filters,
    )
    .await;
    query_ops(
        &mut w,
        &auditor,
        "/api/v1/platform/logs/requests/{root}",
        &format!("/api/v1/platform/logs/requests/{a}"),
        &platform_filters,
    )
    .await;
    query_ops(
        &mut w,
        &auditor,
        "/api/v1/platform/logs/sessions/{ws}/{session}",
        &format!("/api/v1/platform/logs/sessions/{}/s-rich", f.team),
        &platform_session,
    )
    .await;
    query_ops(
        &mut w,
        &owner,
        "/api/v1/workspaces/{ws}/generations",
        &format!("{ws}/generations"),
        &workspace_logs,
    )
    .await;
    query_ops(
        &mut w,
        &owner,
        "/api/v1/workspaces/{ws}/requests",
        &format!("{ws}/requests"),
        &workspace_logs,
    )
    .await;
    query_ops(
        &mut w,
        &owner,
        "/api/v1/workspaces/{ws}/logs/metrics",
        &format!("{ws}/logs/metrics"),
        &no_page,
    )
    .await;
    query_ops(
        &mut w,
        &owner,
        "/api/v1/workspaces/{ws}/requests/{root}",
        &format!("{ws}/requests/{a}"),
        &no_page,
    )
    .await;
    let first_file = fileid.clone();
    query_ops(
        &mut w,
        &owner,
        "/api/v1/workspaces/{ws}/files",
        &format!("{ws}/files"),
        &[
            ("purpose", "user_data"),
            ("search", "contract"),
            ("limit", "50"),
            ("after", &first_file),
        ],
    )
    .await;
    query_ops(
        &mut w,
        &owner,
        "/api/v1/workspaces/{ws}/executions",
        &format!("{ws}/executions"),
        &[("limit", "50"), ("offset", "0")],
    )
    .await;
    query_ops(
        &mut w,
        &owner,
        "/api/v1/workspaces/{ws}/audit",
        &format!("{ws}/audit"),
        &[("limit", "50"), ("offset", "0")],
    )
    .await;
    query_ops(
        &mut w,
        &auditor,
        "/api/v1/platform/audit",
        "/api/v1/platform/audit",
        &[
            ("limit", "50"),
            ("offset", "0"),
            ("actor_user_id", &f.admin.user_id.to_string()),
            ("hide_sign_ins", "true"),
            ("exclude_actions", "key.created,policy.type_updated"),
        ],
    )
    .await;
    query_ops(
        &mut w,
        &owner,
        "/api/v1/workspaces/{ws}/batches",
        &format!("{ws}/batches"),
        &[("status", "all"), ("limit", "50"), ("offset", "0")],
    )
    .await;
    query_ops(
        &mut w,
        &auditor,
        "/api/v1/platform/batches",
        "/api/v1/platform/batches",
        &[("status", "all"), ("limit", "50"), ("offset", "0")],
    )
    .await;
    query_ops(
        &mut w,
        &auditor,
        "/api/v1/platform/legal-holds",
        "/api/v1/platform/legal-holds",
        &[("state", "all")],
    )
    .await;
    query_ops(
        &mut w,
        &admin,
        "/api/v1/platform/admin-tokens",
        "/api/v1/platform/admin-tokens",
        &[("limit", "50"), ("offset", "0")],
    )
    .await;
    query_ops(
        &mut w,
        &owner,
        "/api/v1/workspaces/{ws}/service-accounts",
        &format!("{ws}/service-accounts"),
        &[("limit", "50"), ("offset", "0")],
    )
    .await;
    query_ops(
        &mut w,
        &owner,
        "/api/v1/workspaces/{ws}/keys",
        &format!("{ws}/keys"),
        &[("limit", "50"), ("offset", "0"), ("q", "key")],
    )
    .await;
    query_ops(
        &mut w,
        &owner,
        "/api/v1/workspaces/{ws}/keys/{key}/access",
        &format!("{ws}/keys/{full_key}/access"),
        &[("model_id", &m.to_string())],
    )
    .await;
    query_ops(
        &mut w,
        &owner,
        "/api/v1/workspaces/{ws}/key-safety",
        &format!("{ws}/key-safety"),
        &[
            ("unused_days", "30"),
            ("rotation_days", "180"),
            ("key_id", &full_key.to_string()),
        ],
    )
    .await;
    query_ops(
        &mut w,
        &auditor,
        "/api/v1/platform/key-safety",
        "/api/v1/platform/key-safety",
        &[
            ("unused_days", "30"),
            ("rotation_days", "180"),
            ("workspace_id", &team),
        ],
    )
    .await;

    // ... and of every JSON write, each on fresh state in the project workspace.
    let valid_key = json!({"name":"contract","expires_in_days":30,"service_account_id":null,"model_ids":[m],"requests_per_minute":100,"tokens_per_minute":200,"concurrent_requests":5,"concurrent_jobs":2,"budgets":[{"period":"month","amount_microusd":"5000000"}]});
    body_ops(
        &mut w,
        &owner,
        "POST",
        "/api/v1/workspaces/{ws}/keys",
        &format!("{project}/keys"),
        valid_key,
    )
    .await;
    let (client, root) = (w.client.clone(), w.base.clone());
    let fresh_key = async || {
        let k = raw(
            &client,
            &root,
            &owner,
            "POST",
            &format!("{project}/keys"),
            Some(json!({"name":"vector","expires_in_days":30})),
        )
        .await;
        assert_eq!(k.status, StatusCode::OK);
        id(&serde_json::from_slice::<Value>(&k.bytes).unwrap())
    };
    let vector_key = fresh_key().await;
    body_ops(
        &mut w,
        &owner,
        "PATCH",
        "/api/v1/workspaces/{ws}/keys/{key}",
        &format!("{project}/keys/{vector_key}"),
        json!({"disabled":true}),
    )
    .await;
    body_ops(
        &mut w,
        &owner,
        "POST",
        "/api/v1/workspaces/{ws}/keys/{key}/rotate",
        &format!("{project}/keys/{vector_key}/rotate"),
        json!({"expires_in_days":30}),
    )
    .await;
    // The policy write is stored-cap monotone: every request needs its own key.
    let valid_policy = json!({"requests_per_minute":100,"tokens_per_minute":30,"concurrent_requests":2,"concurrent_jobs":1,"storage_bytes":null,"key_issuances_per_holder":null,"key_issuances_per_workspace":null,"budgets":[{"period":"month","amount_microusd":"5000000"}]});
    {
        let (client, root) = (w.client.clone(), w.base.clone());
        let failed = std::panic::AssertUnwindSafe(body_vectors_http(
            &mut w.observed,
            "PUT",
            "/api/v1/workspaces/{ws}/keys/{key}/policy",
            valid_policy,
            async |body: Value| {
                let fresh = fresh_key().await;
                probe(
                    raw(
                        &client,
                        &root,
                        &owner,
                        "PUT",
                        &format!("{project}/keys/{fresh}/policy"),
                        Some(body),
                    )
                    .await,
                )
            },
        ))
        .catch_unwind()
        .await
        .is_err();
        if failed {
            w.failed.push("PUT policy".to_owned());
        }
    }
    body_ops(
        &mut w,
        &owner,
        "POST",
        "/api/v1/workspaces/{ws}/service-accounts",
        &format!("{project}/service-accounts"),
        json!({"name":"contract"}),
    )
    .await;
    let project_account = id(&w
        .json(
            &owner,
            "POST",
            &format!("{project}/service-accounts"),
            Some(json!({"name":"vector account"})),
        )
        .await);
    body_ops(
        &mut w,
        &owner,
        "PATCH",
        "/api/v1/workspaces/{ws}/service-accounts/{id}",
        &format!("{project}/service-accounts/{project_account}"),
        json!({"disabled":false}),
    )
    .await;
    body_ops(
        &mut w,
        &admin,
        "POST",
        "/api/v1/platform/legal-holds",
        "/api/v1/platform/legal-holds",
        json!({"user_id":null,"workspace_id":f.project,"reference":"CONTRACT-V"}),
    )
    .await;
    body_ops(&mut w, &admin, "POST", "/api/v1/platform/admin-tokens", "/api/v1/platform/admin-tokens", json!({"name":"contract","scopes":["platform:read"],"expires_in_days":30,"ip_allowlist":["127.0.0.1/32"]})).await;

    // Actual issuance refusal, with no minted credential and a bounded wait.
    w.json(&owner, "GET", &format!("{ws}/key-issuance"), None)
        .await;
    sqlx::query("INSERT INTO workspace_local_policies(workspace_id,key_issuances_per_holder,key_issuances_per_workspace) VALUES($1,1,1) ON CONFLICT(workspace_id) DO UPDATE SET key_issuances_per_holder=1,key_issuances_per_workspace=1").bind(f.team).execute(&pool).await.unwrap();
    let full_window = w
        .json(&owner, "GET", &format!("{ws}/key-issuance"), None)
        .await;
    assert!(full_window["holder"]["next_available_at"].is_string());
    assert!(full_window["workspace"]["next_available_at"].is_string());
    let r = w
        .send(
            &owner,
            "POST",
            &format!("{ws}/keys"),
            Some(json!({"name":"Refused issuance","expires_in_days":30})),
        )
        .await;
    assert_eq!(r.status, StatusCode::TOO_MANY_REQUESTS);
    let refusal: Value = serde_json::from_slice(&r.bytes).unwrap();
    assert_eq!(refusal["error"]["reason"], "key_issuance_limit_exceeded");
    assert_eq!(refusal["error"]["scope"], "holder");
    assert_eq!(
        r.headers["retry-after"]
            .to_str()
            .unwrap()
            .parse::<u64>()
            .unwrap(),
        refusal["error"]["retry_after_seconds"].as_u64().unwrap()
    );
    // Multipart has no serde DTO and no JSON-derived vectors
    // (`x-omg-no-boundary-vectors`): every part-level case is a real multipart
    // request whose handler verdict must equal this schema's verdict on the
    // same parts. Duplicate parts cannot be written as a JSON object, so that
    // case alone is checked against the handler only.
    let upload_schema = openapi::responses::artifact()["components"]["schemas"]
        ["post_api_v1_workspaces_by_ws_files_uploadRequest"]
        .clone();
    let (png, jsonl, text): (&[u8], &[u8], &[u8]) = (
        b"\x89PNG\r\n\x1a\nbody",
        b"{\"custom_id\":\"contract\"}\n",
        b"content",
    );
    let expiring = |seconds: &'static str| {
        vec![
            ("purpose", "user_data"),
            ("expires_after[anchor]", "created_at"),
            ("expires_after[seconds]", seconds),
        ]
    };
    type Case = (
        &'static str,
        Vec<(&'static str, &'static str)>,
        Option<&'static [u8]>,
        StatusCode,
        bool,
    );
    let ok = StatusCode::OK;
    let bad = StatusCode::BAD_REQUEST;
    let mut cases: Vec<Case> = vec![
        (
            "purpose batch",
            vec![("purpose", "batch")],
            Some(jsonl),
            ok,
            true,
        ),
        (
            "purpose user_data",
            vec![("purpose", "user_data")],
            Some(text),
            ok,
            true,
        ),
        (
            "purpose assistants",
            vec![("purpose", "assistants")],
            Some(text),
            ok,
            true,
        ),
        (
            "purpose vision",
            vec![("purpose", "vision")],
            Some(png),
            ok,
            true,
        ),
        (
            "purpose evals",
            vec![("purpose", "evals")],
            Some(text),
            ok,
            true,
        ),
        (
            "purpose batch_output",
            vec![("purpose", "batch_output")],
            Some(text),
            bad,
            true,
        ),
        (
            "purpose fine-tune",
            vec![("purpose", "fine-tune")],
            Some(text),
            bad,
            true,
        ),
        (
            "purpose unknown",
            vec![("purpose", "bogus")],
            Some(text),
            bad,
            true,
        ),
        (
            "purpose padded",
            vec![("purpose", "user_data ")],
            Some(text),
            bad,
            true,
        ),
        (
            "purpose case",
            vec![("purpose", "User_Data")],
            Some(text),
            bad,
            true,
        ),
        ("purpose missing", vec![], Some(text), bad, true),
        (
            "file missing",
            vec![("purpose", "user_data")],
            None,
            bad,
            true,
        ),
        (
            "file empty",
            vec![("purpose", "user_data")],
            Some(b""),
            bad,
            true,
        ),
        (
            "unknown field",
            vec![
                ("purpose", "user_data"),
                ("unexpected_configuration", "true"),
            ],
            Some(text),
            bad,
            true,
        ),
        (
            "duplicate purpose",
            vec![("purpose", "user_data"), ("purpose", "user_data")],
            Some(text),
            bad,
            false,
        ),
        (
            "anchor without seconds",
            vec![
                ("purpose", "user_data"),
                ("expires_after[anchor]", "created_at"),
            ],
            Some(text),
            bad,
            true,
        ),
        (
            "seconds without anchor",
            vec![("purpose", "user_data"), ("expires_after[seconds]", "3600")],
            Some(text),
            bad,
            true,
        ),
        (
            "anchor other",
            vec![
                ("purpose", "user_data"),
                ("expires_after[anchor]", "updated_at"),
                ("expires_after[seconds]", "3600"),
            ],
            Some(text),
            bad,
            true,
        ),
    ];
    for (label, seconds, want) in [
        ("seconds 3599", "3599", bad),
        ("seconds 3600", "3600", ok),
        ("seconds 2592000", "2592000", ok),
        ("seconds 2592001", "2592001", bad),
        ("seconds 99999999999999999999", "99999999999999999999", bad),
        ("seconds padded", " 3600 ", ok),
        ("seconds plus and zeros", "+03600", ok),
        ("seconds zero", "0", bad),
        ("seconds negative", "-3600", bad),
        ("seconds fractional", "3600.5", bad),
        ("seconds non-numeric", "null", bad),
        ("seconds empty", "", bad),
    ] {
        cases.push((label, expiring(seconds), Some(text), want, true));
    }
    for (label, fields, file, want, comparable) in cases {
        let r = upload(&mut w, &owner, &uploadpath, &fields, file, true).await;
        assert_eq!(r.status, want, "upload case {label}");
        if comparable {
            let mut parts = serde_json::Map::new();
            for (name, value) in &fields {
                parts.insert((*name).to_owned(), json!(value));
            }
            if let Some(file) = file {
                parts.insert("file".into(), json!(if file.is_empty() { "" } else { "x" }));
            }
            let schema_accepts = openapi::responses::validate_value(
                openapi::responses::artifact(),
                &upload_schema,
                &Value::Object(parts),
            )
            .is_ok();
            assert_eq!(
                schema_accepts,
                r.status.is_success(),
                "upload case {label}: schema and handler disagree"
            );
        }
    }
    // The storage quota answers 413 for an input the schema accepts. The
    // per-file cap (`GATEWAY_FILES_MAX_BYTES`, a process-wide setting) is
    // unit-tested in `filestore/api_tests.rs`.
    sqlx::query("INSERT INTO workspace_local_policies(workspace_id,storage_bytes) VALUES($1,1) ON CONFLICT(workspace_id) DO UPDATE SET storage_bytes=1").bind(f.project).execute(&pool).await.unwrap();
    let refused = upload(
        &mut w,
        &owner,
        &format!("{project}/files/upload"),
        &[("purpose", "user_data")],
        Some(text),
        true,
    )
    .await;
    assert_eq!(refused.status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(
        serde_json::from_slice::<Value>(&refused.bytes).unwrap()["error"]["reason"],
        "storage_quota_exceeded"
    );
    // Request rules the derived vectors cannot reach: each is sent over HTTP and
    // the contract's verdict is compared with the handler's. `false` marks a
    // rule JSON Schema cannot express (documented in the schema's description):
    // the contract accepts, the handler refuses.
    let keys_path = format!("{ws}/keys");
    let keys_template = "/api/v1/workspaces/{ws}/keys";
    let policy_template = "/api/v1/workspaces/{ws}/keys/{key}/policy";
    let key_body = |extra: Value| {
        let mut body = json!({"name":"Vector key","expires_in_days":30});
        body.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        body
    };
    let multibyte = |n: usize| "\u{e9}".repeat(n);
    for (label, amount, expressible) in [
        ("money above i64", "9999999999999999999", true),
        ("money just above i64", "9223372036854775808", true),
        ("money zero", "0", true),
    ] {
        let budgets = json!([{"period":"month","amount_microusd":amount}]);
        body_vector(
            &w,
            &owner,
            "POST",
            keys_template,
            &keys_path,
            label,
            key_body(json!({"budgets":budgets})),
            expressible,
        )
        .await;
        let policy = json!({"requests_per_minute":null,"tokens_per_minute":null,"concurrent_requests":null,"budgets":budgets});
        body_vector(
            &w,
            &owner,
            "PUT",
            policy_template,
            &format!("{keypath}/policy"),
            label,
            policy,
            expressible,
        )
        .await;
    }
    let repeated =
        json!([{"period":"month","amount_microusd":"1"},{"period":"month","amount_microusd":"2"}]);
    body_vector(
        &w,
        &owner,
        "POST",
        keys_template,
        &keys_path,
        "duplicate period",
        key_body(json!({"budgets":repeated})),
        false,
    )
    .await;
    body_vector(&w, &owner, "PUT", policy_template, &format!("{keypath}/policy"), "duplicate period", json!({"requests_per_minute":null,"tokens_per_minute":null,"concurrent_requests":null,"budgets":repeated}), false).await;
    // Names are limited in UTF-8 bytes; maxLength counts characters.
    let sa_path = format!("{ws}/service-accounts");
    let sa_template = "/api/v1/workspaces/{ws}/service-accounts";
    body_vector(
        &w,
        &owner,
        "POST",
        keys_template,
        &keys_path,
        "key name 120 bytes",
        json!({"name":multibyte(60),"expires_in_days":30}),
        true,
    )
    .await;
    body_vector(
        &w,
        &owner,
        "POST",
        keys_template,
        &keys_path,
        "key name 122 bytes",
        json!({"name":multibyte(61),"expires_in_days":30}),
        false,
    )
    .await;
    body_vector(
        &w,
        &owner,
        "POST",
        sa_template,
        &sa_path,
        "service account name 120 bytes",
        json!({"name":multibyte(60)}),
        true,
    )
    .await;
    body_vector(
        &w,
        &owner,
        "POST",
        sa_template,
        &sa_path,
        "service account name 122 bytes",
        json!({"name":multibyte(61)}),
        false,
    )
    .await;
    // Admin token allowlists.
    let tokens = "/api/v1/platform/admin-tokens";
    for (label, entry, expressible) in [
        ("allowlist bad octet", "999.1.1.1", true),
        ("allowlist leading zero", "010.0.0.0/8", true),
        ("allowlist v4 prefix 33", "10.0.0.0/33", true),
        ("allowlist v4 prefix 0", "10.0.0.0/0", true),
        ("allowlist v6 prefix 129", "::1/129", true),
        ("allowlist v6 prefix 0", "::/0", true),
        ("allowlist not an address", "example.invalid", true),
        ("allowlist v4 network", "10.0.0.0/8", true),
        ("allowlist v4 address", "192.0.2.1", true),
        ("allowlist v6 network", "2001:db8::/32", true),
        ("allowlist host bits", "10.0.0.1/8", false),
        ("allowlist mapped v6", "::ffff:102:304", false),
    ] {
        let body = json!({"name":"vector","scopes":["platform:read"],"expires_in_days":30,"ip_allowlist":[entry]});
        body_vector(&w, &admin, "POST", tokens, tokens, label, body, expressible).await;
    }
    // String query parameters.
    let requests = "/api/v1/platform/logs/requests";
    let sessions = "/api/v1/platform/logs/sessions";
    let uuid = "00000000-0000-4000-8000-000000000000";
    for (label, name, value, expressible) in [
        ("model control", "model", "a\u{1}b".to_owned(), true),
        ("model 200 bytes", "model", multibyte(100), true),
        ("model 202 bytes", "model", multibyte(101), false),
        ("session leading space", "session_id", " a".to_owned(), true),
        (
            "session trailing space",
            "session_id",
            "a ".to_owned(),
            true,
        ),
        ("session control", "session_id", "a\u{1}b".to_owned(), true),
        ("session inner space", "session_id", "a b".to_owned(), true),
        ("session 128 characters", "session_id", multibyte(128), true),
    ] {
        query_vector(
            &w,
            &admin,
            "/api/v1/platform/logs/requests",
            requests,
            name,
            label,
            &value,
            expressible,
        )
        .await;
    }
    for (label, value, expressible) in [
        ("cursor odd hex", format!("1_{uuid}_616"), true),
        ("cursor even hex", format!("1_{uuid}_6162"), true),
        ("cursor not utf-8", format!("1_{uuid}_ff"), false),
    ] {
        query_vector(
            &w,
            &admin,
            "/api/v1/platform/logs/sessions",
            sessions,
            "cursor",
            label,
            &value,
            expressible,
        )
        .await;
    }
    for (label, value) in [
        ("keys q control", "a\u{1}b"),
        ("keys q nul", "abc\u{0}"),
        ("keys q padded", " \n abc\t"),
        ("keys q 200", &"a".repeat(200)),
        ("keys q 201", &"a".repeat(201)),
    ] {
        query_vector(
            &w,
            &owner,
            keys_template,
            &keys_path,
            "q",
            label,
            value,
            true,
        )
        .await;
    }
    // Successful request controls precede extractor refusal probes.
    w.status(
        &owner,
        "POST",
        &format!("{ws}/keys"),
        Some(json!({"name":"bad","expires_in_days":30,"unknown":true})),
        StatusCode::UNPROCESSABLE_ENTITY,
    )
    .await;
    w.status(&owner,"PUT",&format!("{keypath}/policy"),Some(json!({"requests_per_minute":null,"tokens_per_minute":null,"concurrent_requests":null,"budgets":[{"period":"month","amount_microusd":"1","unknown":true}]})),StatusCode::UNPROCESSABLE_ENTITY).await;
    // Every vector and coverage gap is reported by one run (panic messages
    // name only operations, kinds, pointers and statuses).
    let coverage = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        w.observed.finish(openapi::responses::SLICE6_AREAS)
    }));
    assert!(
        w.failed.is_empty() && coverage.is_ok(),
        "slice 6 contract gaps: vectors failed for {:?}; coverage gate passed: {}",
        w.failed,
        coverage.is_ok()
    );
}
