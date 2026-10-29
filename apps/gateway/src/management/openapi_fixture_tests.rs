//! Executed coverage, not a list of claimed fixture names. Every observation
//! comes from a real handler; this suite also runs the gateway over loopback.
use super::*;
use openapi::{
    http_vectors::{
        Probe, body_vectors_http, body_vectors_seeded, query_vectors, query_vectors_fixed,
    },
    responses::{Captured, PositiveFixtures, capture_http},
};
use sha2::{Digest, Sha256};

const ORIGIN: &str = "http://127.0.0.1:3000";
const MIB: usize = 1024 * 1024;
struct Server(tokio::task::JoinHandle<()>);
impl Drop for Server {
    fn drop(&mut self) {
        self.0.abort();
    }
}
struct Browser {
    token: String,
    csrf: String,
    id: Uuid,
}
/// A live browser session. The rows are what the OIDC callback writes
/// (identity.rs `callback`): a device label and `last_seen_at` on the session
/// and the user's sign-in times. `legacy` is a session created before
/// migration 0041, which has no device label or `last_seen_at` until first used.
async fn browser(pool: &PgPool, user: &BrowserPrincipal, legacy: bool) -> Browser {
    let token = crate::entropy::hex_token().unwrap();
    let csrf = crate::entropy::hex_token().unwrap();
    let sql = if legacy {
        "INSERT INTO browser_sessions(token_hash,user_id,csrf_hash,expires_at,verified_email) VALUES($1,$2,$3,now()+interval '1 hour',$4) RETURNING id"
    } else {
        "INSERT INTO browser_sessions(token_hash,user_id,csrf_hash,expires_at,verified_email,client_label,last_seen_at) VALUES($1,$2,$3,now()+interval '1 hour',$4,'Firefox on Linux',now()) RETURNING id"
    };
    let id = sqlx::query_scalar(sql)
        .bind(Sha256::digest(token.as_bytes()).as_slice())
        .bind(user.user_id)
        .bind(Sha256::digest(csrf.as_bytes()).as_slice())
        .bind(&user.email)
        .fetch_one(pool)
        .await
        .unwrap();
    sqlx::query("UPDATE users SET first_signed_in_at=coalesce(first_signed_in_at,now()),last_signed_in_at=now() WHERE id=$1")
        .bind(user.user_id)
        .execute(pool)
        .await
        .unwrap();
    Browser { token, csrf, id }
}
async fn send(
    client: &reqwest::Client,
    base: &str,
    browser: Option<&Browser>,
    method: &str,
    path: &str,
    body: Option<Value>,
) -> Captured {
    let mut request = client
        .request(method.parse().unwrap(), format!("{base}{path}"))
        .header("origin", ORIGIN);
    if let Some(b) = browser {
        request = request
            .header("cookie", format!("omg_session={}", b.token))
            .header("x-csrf-token", &b.csrf);
    }
    if let Some(body) = body {
        request = request.json(&body);
    }
    capture_http(method, path, request.send().await.unwrap()).await
}
/// A request target with a raw query string (empty: none).
fn target(path: &str, query: &str) -> String {
    if query.is_empty() {
        path.to_owned()
    } else {
        format!("{path}?{query}")
    }
}
fn body(captured: &Captured) -> Value {
    assert_eq!(captured.status, StatusCode::OK);
    serde_json::from_slice(&captured.bytes).unwrap()
}
/// A Files API upload (multipart) with a key as the credential.
async fn upload(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    fields: &[(&str, &str)],
    filename: &str,
    part_type: Option<&str>,
    data: &[u8],
) -> Value {
    const BOUNDARY: &str = "contractBoundary7";
    let mut out = Vec::new();
    for (name, value) in fields {
        out.extend_from_slice(
            format!(
                "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"
            )
            .as_bytes(),
        );
    }
    let part = part_type.map_or(String::new(), |t| format!("Content-Type: {t}\r\n"));
    out.extend_from_slice(format!("--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\n{part}\r\n").as_bytes());
    out.extend_from_slice(data);
    out.extend_from_slice(format!("\r\n--{BOUNDARY}--\r\n").as_bytes());
    let response = client
        .post(format!("{base}/v1/files"))
        .header("authorization", format!("Bearer {token}"))
        .header(
            "content-type",
            format!("multipart/form-data; boundary={BOUNDARY}"),
        )
        .body(out)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    response.json().await.unwrap()
}
/// One incident of a workspace rule, as the evaluator writes it, and its first
/// (fired) delivery. `stale` incidents fired outside the 90-day feed window.
async fn workspace_incident(
    pool: &PgPool,
    rule: Uuid,
    ws: Uuid,
    subject: &str,
    stale: bool,
) -> Uuid {
    let event: Uuid = sqlx::query_scalar("INSERT INTO alert_events(id,rule_id,kind,subject_key,level,severity,workspace_id,summary,fired_at) VALUES(gen_random_uuid(),$1,'budget_threshold',$2,100,'critical',$3,'Workspace budget reached',CASE WHEN $4 THEN clock_timestamp()-interval '100 days' ELSE clock_timestamp() END) RETURNING id")
        .bind(rule).bind(subject).bind(ws).bind(stale).fetch_one(pool).await.unwrap();
    sqlx::query(
        "INSERT INTO alert_deliveries(id,event_id,transition) VALUES(gen_random_uuid(),$1,'fired')",
    )
    .bind(event)
    .execute(pool)
    .await
    .unwrap();
    event
}
/// What the recipient outbox's `refresh` stores for one recipient whose first
/// send timed out and is waiting to be retried (alerts/outbox.rs).
async fn retrying_delivery(pool: &PgPool, event: Uuid, transition: &str) {
    sqlx::query("UPDATE alert_deliveries SET recipients=1,error='timeout',attempt_count=1,retrying=true,next_attempt_at=clock_timestamp()+interval '10 minutes',waiting=1 WHERE event_id=$1 AND transition=$2")
        .bind(event).bind(transition).execute(pool).await.unwrap();
}
async fn resolve(pool: &PgPool, event: Uuid) {
    let mut tx = pool.begin().await.unwrap();
    crate::alerts::resolve(&mut tx, event, "cleared")
        .await
        .unwrap();
    tx.commit().await.unwrap();
}
/// A ceiling incident exactly as `alerts::ceiling_conditions` writes it.
async fn ceiling_incident(pool: &PgPool, rule: Uuid, ws: Uuid, details: Value) {
    sqlx::query("INSERT INTO alert_events(id,rule_id,kind,subject_key,level,severity,workspace_id,summary,details) VALUES($1,$2,'admission_ceiling',$3,1,'warning',$4,'A workspace is near its admission ceiling',$5)")
        .bind(Uuid::new_v4()).bind(rule).bind(format!("workspace:{ws}")).bind(ws).bind(details).execute(pool).await.unwrap();
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn management_openapi_completed_area_fixtures(pool: PgPool) {
    let mut observed = PositiveFixtures::default();
    crate::identity::tests::management_openapi_auth_fixtures(&pool, &mut observed).await;
    let f = fixture(&pool).await;
    let owner = browser(&pool, &f.owner, false).await;
    let requester = browser(&pool, &f.outsider, false).await;
    let second = browser(&pool, &f.member, false).await;
    let admin = browser(&pool, &f.admin, false).await;
    let auditor = browser(&pool, &f.auditor, false).await;
    // Sessions that predate migration 0041 and have not been used since.
    let other_session = browser(&pool, &f.owner, true).await;
    let keep_for_revoke_others = browser(&pool, &f.owner, true).await;
    let foreign_personal =
        workspace(&pool, "Foreign private", "personal", f.outsider.user_id).await;
    // Discoverable shared workspaces the requesters can ask to join.
    let lab = workspace(&pool, "Open Lab", "team", f.admin.user_id).await;
    let lab_two = workspace(&pool, "Open Lab Two", "project", f.admin.user_id).await;
    for ws in [f.team, lab, lab_two] {
        sqlx::query("UPDATE workspaces SET discoverable=true WHERE id=$1")
            .bind(ws)
            .execute(&pool)
            .await
            .unwrap();
    }
    let serving = model(&pool, "contract-serving").await;
    let dark = model(&pool, "contract-not-serving").await;
    for model in [serving, dark] {
        direct(&pool, f.team, model).await;
    }
    let connection = Uuid::new_v4();
    let deployment = Uuid::new_v4();
    sqlx::query("INSERT INTO provider_connections(id,name,provider,credential_ref,enabled) VALUES($1,'Contract mock','openai','env:UNUSED',true)").bind(connection).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO deployments(id,model_id,provider_connection_id,upstream_model,enabled) VALUES($1,$2,$3,'mock',true)").bind(deployment).bind(serving).bind(connection).execute(&pool).await.unwrap();
    let (status, key_value) = key(&f, &f.owner, f.team, Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    let own_key = id(&key_value);
    let (status, foreign_key) = key(&f, &f.member, f.team, Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    // Real persisted, nonempty request/export records: an execution whose cost
    // is unknown (no reservation), a settled one, and an upstream failure.
    let execution = Uuid::new_v4();
    sqlx::query("INSERT INTO inference_executions(id,workspace_id,api_key_id,deployment_id,public_model,provider,streamed,state,root_request_id,input_tokens,output_tokens) VALUES($1,$2,$3,$4,'contract-serving','openai',false,'succeeded',$1,5,1)").bind(execution).bind(f.team).bind(own_key).bind(deployment).execute(&pool).await.unwrap();
    let settled = Uuid::new_v4();
    sqlx::query("INSERT INTO inference_executions(id,workspace_id,api_key_id,deployment_id,public_model,provider,streamed,state,root_request_id,input_tokens,output_tokens,completed_at,client_app,client_session_id,elapsed_ms) VALUES($1,$2,$3,$4,'contract-serving','openai',false,'succeeded',$1,100,20,now(),'contract-cli','session-contract',120)").bind(settled).bind(f.team).bind(own_key).bind(deployment).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO governance_reservations(execution_id,workspace_id,api_key_id,deployment_id,admitted_at,minute_start,month_start,lease_expires_at,state,actual_microusd,held_microusd,input_tokens,output_tokens) SELECT e.id,e.workspace_id,e.api_key_id,e.deployment_id,e.started_at,date_trunc('minute',e.started_at,'UTC'),date_trunc('month',e.started_at,'UTC'),now()-interval '1 day','settled',70,0,100,20 FROM inference_executions e WHERE e.id=$1").bind(settled).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO inference_executions(id,workspace_id,api_key_id,deployment_id,public_model,provider,streamed,state,error_code,root_request_id,completed_at) VALUES($1,$2,$3,$4,'contract-serving','openai',false,'failed','timeout_error',$1,now())").bind(Uuid::new_v4()).bind(f.team).bind(own_key).bind(deployment).execute(&pool).await.unwrap();
    // Stacked platform budgets for Team workspaces, one for Projects.
    let (status, _) = call(&f.s, &f.admin, "PUT", "/api/v1/platform/workspace-types/team/policy", json!({"requests_per_minute":null,"tokens_per_minute":null,"concurrent_requests":null,"budgets":[{"period":"month","amount_microusd":"9007199254740993"},{"period":"day","amount_microusd":"1000000"},{"period":"lifetime","amount_microusd":"9007199254740993"}]})).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = call(&f.s, &f.admin, "PUT", "/api/v1/platform/workspace-types/project/policy", json!({"requests_per_minute":null,"tokens_per_minute":null,"concurrent_requests":null,"budgets":[{"period":"month","amount_microusd":"5000000"}]})).await;
    assert_eq!(status, StatusCode::OK);
    // An administrator's workspace whose day accounting has drifted: the
    // summary must report that window as unknown, never as zero.
    let (status, lab_key) = key(&f, &f.admin, lab, Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    sqlx::query("INSERT INTO inference_executions(id,workspace_id,api_key_id,deployment_id,public_model,provider,streamed,state,root_request_id,input_tokens,output_tokens) VALUES($1,$2,$3,$4,'contract-serving','openai',false,'succeeded',$1,1,1)").bind(Uuid::new_v4()).bind(lab).bind(id(&lab_key)).bind(deployment).execute(&pool).await.unwrap();
    sqlx::query("UPDATE budget_totals SET settled_microusd=-1,held_microusd=0 WHERE scope_kind='workspace' AND scope_id=$1 AND period='day'")
        .bind(lab).execute(&pool).await.unwrap();
    // Alert rules: workspace budget (the owner's feed), provider failure, three
    // admission-ceiling shapes (rate and lock wait, rate only, lock wait only).
    let (status, workspace_rule) = call(&f.s, &f.owner, "POST", &format!("/api/v1/workspaces/{}/alerts/rules", f.team), json!({"name":"Generation","kind":"budget_threshold","budget_layers":["local"],"thresholds":[100],"notify_workspace_admins":true})).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = call(&f.s, &f.admin, "POST", "/api/v1/platform/alerts/rules", json!({"name":"Provider failing","kind":"provider_failing","window_minutes":60,"consecutive_failures":1,"notify_platform_admins":true})).await;
    assert_eq!(status, StatusCode::OK);
    sqlx::query("UPDATE api_keys SET expires_at=now()+interval '3 days'")
        .execute(&pool)
        .await
        .unwrap();
    crate::alerts::evaluate_once(&f.s).await.unwrap().unwrap();
    let (status, ceiling) = call(&f.s, &f.admin, "POST", "/api/v1/platform/alerts/rules", json!({"name":"Contract ceiling","kind":"admission_ceiling","window_minutes":5,"ceiling_requests_per_second":200,"ceiling_lock_wait_ms":250,"notify_platform_admins":true})).await;
    assert_eq!(status, StatusCode::OK);
    let (status, rate_only) = call(&f.s, &f.admin, "POST", "/api/v1/platform/alerts/rules", json!({"name":"Ceiling by rate","kind":"admission_ceiling","window_minutes":5,"ceiling_requests_per_second":200,"notify_platform_admins":true})).await;
    assert_eq!(status, StatusCode::OK);
    let (status, wait_only) = call(&f.s, &f.admin, "POST", "/api/v1/platform/alerts/rules", json!({"name":"Ceiling by lock wait","kind":"admission_ceiling","window_minutes":5,"ceiling_lock_wait_ms":250,"notify_platform_admins":true})).await;
    assert_eq!(status, StatusCode::OK);
    let details = |rate: Option<u64>, p95: Option<u64>, wait: Option<u64>| json!({"scope":"workspace","personal":false,"workspace_name":"Team","workspace_kind":"team","window_minutes":5,"admissions":60000,"admissions_per_second":"200.0","by_rate":rate.is_some(),"by_lock_wait":p95.is_some(),"threshold_requests_per_second":rate,"lock_wait_p95_at_least_ms":p95,"threshold_lock_wait_ms":wait});
    ceiling_incident(
        &pool,
        id(&ceiling),
        f.team,
        details(Some(200), None, Some(250)),
    )
    .await;
    ceiling_incident(
        &pool,
        id(&rate_only),
        f.team,
        details(Some(200), None, None),
    )
    .await;
    ceiling_incident(
        &pool,
        id(&wait_only),
        f.team,
        details(None, Some(300), Some(250)),
    )
    .await;
    // alerts::admission_ceiling_conditions suppresses personal references even
    // for Admins: the stored event deliberately has no workspace_id/name.
    sqlx::query("INSERT INTO alert_events(id,rule_id,kind,subject_key,level,severity,summary,details) VALUES($1,$2,'admission_ceiling','contract-personal',1,'warning','An API key in a personal workspace is near its admission ceiling',jsonb_build_object('scope','key','personal',true,'workspace_name',NULL,'workspace_kind','personal','window_minutes',5,'admissions',60000,'admissions_per_second','200.0','by_rate',true,'by_lock_wait',false,'threshold_requests_per_second',200,'lock_wait_p95_at_least_ms',NULL,'threshold_lock_wait_ms',250))")
        .bind(Uuid::new_v4()).bind(id(&ceiling)).execute(&pool).await.unwrap();
    // The owner's workspace-rule incidents. A workspace may generate at most 100
    // email deliveries per window (migration 0059): the first 100 are charged,
    // later ones are stored as `suppressed`.
    let rule = id(&workspace_rule);
    let retried = workspace_incident(&pool, rule, f.team, "retried", false).await;
    resolve(&pool, retried).await;
    retrying_delivery(&pool, retried, "fired").await;
    retrying_delivery(&pool, retried, "resolved").await;
    // Still firing: no clearing delivery yet.
    workspace_incident(&pool, rule, f.team, "firing", false).await;
    let quiet = workspace_incident(&pool, rule, f.team, "quiet", false).await;
    resolve(&pool, quiet).await;
    // Fill the rest of the Team's allowance with incidents outside the feed window.
    let charged: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM alert_deliveries WHERE bucket_id=$1 AND quota_charged",
    )
    .bind(f.team)
    .fetch_one(&pool)
    .await
    .unwrap();
    for i in 0..(100 - charged) {
        workspace_incident(&pool, rule, f.team, &format!("stale-{i}"), true).await;
    }
    let suppressed = workspace_incident(&pool, rule, f.team, "suppressed", false).await;
    resolve(&pool, suppressed).await;
    let counts: (i64, i64) = sqlx::query_as("SELECT count(*) FILTER(WHERE status='suppressed' AND suppression_reason='workspace_generation_limit'),count(*) FILTER(WHERE quota_charged) FROM alert_deliveries WHERE bucket_id=$1")
        .bind(f.team).fetch_one(&pool).await.unwrap();
    assert_eq!(counts, (2, 100));
    // The owner's keys: restricted to a model, with a lifetime budget, disabled,
    // revoked, and one with no expiry exactly as bootstrap writes it.
    let (status, restricted) = key(&f, &f.owner, f.team, json!([serving])).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(restricted["model_ids"], json!([serving]));
    let (status, _) = call(&f.s, &f.owner, "POST", &format!("/api/v1/workspaces/{}/keys", f.team), json!({"name":"Budgeted","expires_in_days":1,"budgets":[{"period":"lifetime","amount_microusd":"5000000"}]})).await;
    assert_eq!(status, StatusCode::OK);
    let (status, to_disable) = key(&f, &f.owner, f.team, Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = call(
        &f.s,
        &f.owner,
        "PATCH",
        &format!("/api/v1/workspaces/{}/keys/{}", f.team, id(&to_disable)),
        json!({"disabled":true}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, to_revoke) = key(&f, &f.owner, f.team, Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = call(
        &f.s,
        &f.owner,
        "DELETE",
        &format!("/api/v1/workspaces/{}/keys/{}", f.team, id(&to_revoke)),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    sqlx::query("INSERT INTO api_keys(id,workspace_id,issued_to_user_id,name,secret_hash) VALUES($1,$2,$3,'Local team key',decode(repeat('ab',32),'hex'))")
        .bind(Uuid::new_v4()).bind(f.team).bind(f.owner.user_id).execute(&pool).await.unwrap();
    // A role the owner held and lost keeps its row (revoked) in their export.
    let (status, _) = call(
        &f.s,
        &f.admin,
        "POST",
        &format!("/api/v1/platform/users/{}/roles", f.owner.user_id),
        json!({"role":"auditor"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = call(
        &f.s,
        &f.admin,
        "DELETE",
        &format!("/api/v1/platform/users/{}/roles/auditor", f.owner.user_id),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, invitation) = call(
        &f.s,
        &f.owner,
        "POST",
        &format!("/api/v1/workspaces/{}/invitations", f.project),
        json!({"email":f.outsider.email,"role":"member"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let mock = crate::identity::tests::MockProvider::start(None).await;
    let identity = mock.state(pool.clone()).await;
    let engine = crate::inference::Engine::new(
        std::sync::Arc::new(f.s.clone()),
        crate::providers::ProviderRegistry::default(),
        crate::inference::EngineLimits::default(),
    )
    .unwrap();
    let app = crate::http::router_with_identity(f.s.clone(), None, engine, identity);
    // One gateway without file storage or a public inference URL, and one with both.
    let runtime = crate::filestore::FileStoreRuntime::memory();
    let configured = app
        .clone()
        .layer(axum::Extension(runtime.clone()))
        .layer(axum::Extension(
            crate::config::InferenceBaseUrl::parse(
                Some("https://api.bitop.dev/v1/"),
                crate::config::Environment::Production,
            )
            .unwrap(),
        ));
    let mut bases = Vec::new();
    let mut servers = Vec::new();
    for router in [app, configured] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        bases.push(format!("http://{}", listener.local_addr().unwrap()));
        servers.push(Server(tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap()
        })));
    }
    let (base, configured_base) = (bases[0].as_str(), bases[1].as_str());
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();

    // Installation presentation, unset and then set through the real API.
    let r = send(&client, base, Some(&owner), "GET", "/api/v1/me", None).await;
    assert!(body(&r)["installation"]["support_url"].is_null());
    observed.observe(&r);
    let (status, _) = call(&f.s, &f.admin, "PUT", "/api/v1/platform/settings/general", json!({"display_name":"Acme AI","support_url":"https://help.example.test/ai","logo_url":"https://cdn.example.test/logo.png","human_key_max_lifetime_days":90})).await;
    assert_eq!(status, StatusCode::OK);
    let r = send(
        &client,
        configured_base,
        Some(&owner),
        "GET",
        "/api/v1/me",
        None,
    )
    .await;
    let configured_me = body(&r);
    assert_eq!(configured_me["installation"]["name"], "Acme AI");
    assert_eq!(
        configured_me["installation"]["inference_base_url"],
        "https://api.bitop.dev/v1"
    );
    assert!(configured_me["installation"]["logo"].is_null());
    observed.observe(&r);
    // Uploading a logo replaces the deprecated external URL.
    branding_tests::management_openapi_branding_fixtures(&f, &runtime, &mut observed).await;
    let r = send(
        &client,
        configured_base,
        Some(&owner),
        "GET",
        "/api/v1/me",
        None,
    )
    .await;
    let configured_me = body(&r);
    assert!(configured_me["installation"]["logo_url"].is_null());
    assert!(configured_me["installation"]["logo"]["url"].is_string());
    observed.observe(&r);
    let r = send(
        &client,
        configured_base,
        None,
        "GET",
        "/api/v1/auth/config",
        None,
    )
    .await;
    let config = body(&r);
    assert_eq!(config["installation_name"], "Acme AI");
    assert!(config["logo"]["url"].is_string());
    observed.observe(&r);
    // Files the owner uploaded through the real Files API.
    sqlx::query(
        "UPDATE installation_settings SET file_batch_enabled=true,file_user_files_enabled=true",
    )
    .execute(&pool)
    .await
    .unwrap();
    let token = key_value["token"].as_str().unwrap();
    let kept = upload(
        &client,
        configured_base,
        token,
        &[
            ("purpose", "user_data"),
            ("expires_after[anchor]", "created_at"),
            ("expires_after[seconds]", "3600"),
        ],
        "notes.txt",
        Some("application/octet-stream"),
        b"hello",
    )
    .await;
    assert!(kept["expires_at"].is_number());
    let removed = upload(
        &client,
        configured_base,
        token,
        &[("purpose", "user_data")],
        "scratch.txt",
        Some("text/plain"),
        b"scratch",
    )
    .await;
    let response = client
        .delete(format!(
            "{configured_base}/v1/files/{}",
            removed["id"].as_str().unwrap()
        ))
        .header("authorization", format!("Bearer {token}"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let r = send(
        &client,
        base,
        Some(&requester),
        "GET",
        "/api/v1/access-request-targets",
        None,
    )
    .await;
    assert!(
        body(&r)["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v["id"] == json!(f.team))
    );
    observed.observe(&r);
    query_vectors_fixed(
        &mut observed,
        "GET",
        "/api/v1/access-request-targets",
        &[("limit", "50"), ("offset", "0")],
        async |query| {
            send(
                &client,
                base,
                Some(&requester),
                "GET",
                &target("/api/v1/access-request-targets", &query),
                None,
            )
            .await
        },
    )
    .await;
    let submission =
        json!({"workspace_id":f.team,"reason":"Join team","idempotency_key":"contract-positive"});
    let r = send(
        &client,
        base,
        Some(&requester),
        "POST",
        "/api/v1/me/access-requests",
        Some(submission.clone()),
    )
    .await;
    let request_id = id(&body(&r));
    assert!(body(&r).get("requester_email").is_none());
    observed.observe(&r);
    for path in [
        "/api/v1/me/access-requests".to_owned(),
        format!("/api/v1/me/access-requests/{request_id}"),
    ] {
        let r = send(&client, base, Some(&requester), "GET", &path, None).await;
        let value = body(&r);
        assert!(!value.to_string().contains("requester_email"));
        if let Some(data) = value.get("data") {
            assert!(!data.as_array().unwrap().is_empty());
        }
        observed.observe(&r);
    }
    let r = send(
        &client,
        base,
        Some(&requester),
        "POST",
        &format!("/api/v1/me/access-requests/{request_id}/withdraw"),
        None,
    )
    .await;
    assert_eq!(body(&r)["status"], "withdrawn");
    assert_eq!(body(&r)["decided_by"], json!(f.outsider.user_id));
    observed.observe(&r);
    let r = send(
        &client,
        base,
        Some(&requester),
        "POST",
        "/api/v1/invitations/accept",
        Some(json!({"token":invitation["token"]})),
    )
    .await;
    assert_eq!(body(&r)["workspace_id"], json!(f.project));
    observed.observe(&r);

    // A second request on the Team, decided by its owner: approved with the
    // membership it created, and replayed with the same idempotency key.
    let vector_submission =
        json!({"workspace_id":f.team,"reason":"Join team","idempotency_key":"contract-vector"});
    body_vectors_http(
        &mut observed,
        "POST",
        "/api/v1/me/access-requests",
        vector_submission.clone(),
        async |request| {
            Probe::from(
                send(
                    &client,
                    base,
                    Some(&requester),
                    "POST",
                    "/api/v1/me/access-requests",
                    Some(request),
                )
                .await,
            )
        },
    )
    .await;
    // The positional wire form [workspace_id, reason, idempotency_key] replays the
    // same pending request (same key and payload) and gets the same boundaries.
    body_vectors_http(
        &mut observed,
        "POST",
        "/api/v1/me/access-requests",
        json!([f.team, "Join team", "contract-vector"]),
        async |request| {
            Probe::from(
                send(
                    &client,
                    base,
                    Some(&requester),
                    "POST",
                    "/api/v1/me/access-requests",
                    Some(request),
                )
                .await,
            )
        },
    )
    .await;
    let approved: Uuid = sqlx::query_scalar(
        "SELECT id FROM access_requests WHERE requester=$1 AND workspace_id=$2 AND status='pending'",
    )
    .bind(f.outsider.user_id)
    .bind(f.team)
    .fetch_one(&pool)
    .await
    .unwrap();
    let (status, decided) = call(
        &f.s,
        &f.owner,
        "POST",
        &format!(
            "/api/v1/workspaces/{}/access-requests/{approved}/approve",
            f.team
        ),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(decided["membership_grant_id"].is_string());
    let r = send(
        &client,
        base,
        Some(&requester),
        "POST",
        "/api/v1/me/access-requests",
        Some(vector_submission),
    )
    .await;
    let replayed = body(&r);
    assert_eq!(replayed["status"], "approved");
    assert_eq!(replayed["id"], json!(approved));
    assert!(replayed["membership_grant_id"].is_string());
    observed.observe(&r);
    let r = send(
        &client,
        base,
        Some(&requester),
        "GET",
        &format!("/api/v1/me/access-requests/{approved}"),
        None,
    )
    .await;
    assert_eq!(body(&r)["decided_by"], json!(f.owner.user_id));
    observed.observe(&r);
    // A member asks for another workspace without a reason: denied by a Platform
    // Admin; and a second request that is withdrawn.
    let denied_submission =
        json!({"workspace_id":lab,"reason":null,"idempotency_key":"contract-denied"});
    let r = send(
        &client,
        base,
        Some(&second),
        "POST",
        "/api/v1/me/access-requests",
        Some(denied_submission),
    )
    .await;
    let denied_id = id(&body(&r));
    assert!(body(&r)["reason"].is_null());
    observed.observe(&r);
    let r = send(
        &client,
        base,
        Some(&second),
        "GET",
        &format!("/api/v1/me/access-requests/{denied_id}"),
        None,
    )
    .await;
    assert_eq!(body(&r)["status"], "pending");
    observed.observe(&r);
    let (status, _) = call(
        &f.s,
        &f.admin,
        "POST",
        &format!("/api/v1/platform/access-requests/{denied_id}/deny"),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let r = send(
        &client,
        base,
        Some(&second),
        "GET",
        &format!("/api/v1/me/access-requests/{denied_id}"),
        None,
    )
    .await;
    assert_eq!(body(&r)["status"], "denied");
    assert!(body(&r)["decided_at"].is_string());
    observed.observe(&r);
    let r = send(
        &client,
        base,
        Some(&second),
        "POST",
        "/api/v1/me/access-requests",
        Some(json!({"workspace_id":lab,"reason":null,"idempotency_key":"contract-withdrawn"})),
    )
    .await;
    let withdrawn_id = id(&body(&r));
    observed.observe(&r);
    let r = send(
        &client,
        base,
        Some(&second),
        "POST",
        &format!("/api/v1/me/access-requests/{withdrawn_id}/withdraw"),
        None,
    )
    .await;
    assert_eq!(body(&r)["status"], "withdrawn");
    assert!(body(&r)["reason"].is_null());
    observed.observe(&r);
    // The requester is removed from the Team again: the export keeps the revoked grant.
    let (status, _) = call(
        &f.s,
        &f.owner,
        "DELETE",
        &format!(
            "/api/v1/workspaces/{}/members/{}",
            f.team, f.outsider.user_id
        ),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    for who in [&requester, &second] {
        let r = send(
            &client,
            base,
            Some(who),
            "GET",
            "/api/v1/me/access-requests",
            None,
        )
        .await;
        assert!(!body(&r)["data"].as_array().unwrap().is_empty());
        observed.observe(&r);
    }
    query_vectors_fixed(
        &mut observed,
        "GET",
        "/api/v1/me/access-requests",
        &[("limit", "50"), ("offset", "0")],
        async |query| {
            send(
                &client,
                base,
                Some(&requester),
                "GET",
                &target("/api/v1/me/access-requests", &query),
                None,
            )
            .await
        },
    )
    .await;

    // A pending request in the positional wire form (reason absent), left undecided.
    let r = send(
        &client,
        base,
        Some(&requester),
        "POST",
        "/api/v1/me/access-requests",
        Some(json!([lab_two, null, "contract-positional"])),
    )
    .await;
    assert_eq!(body(&r)["status"], "pending");
    assert!(body(&r)["reason"].is_null());
    observed.observe(&r);
    // The owner's own scope, every read as the signed-in browser.
    let mut first_event = Value::Null;
    for path in [
        "/api/v1/me",
        "/api/v1/me/summary",
        "/api/v1/me/keys",
        "/api/v1/me/models",
        "/api/v1/me/notifications",
        "/api/v1/me/notifications/summary",
        "/api/v1/me/sessions",
        "/api/v1/me/export",
    ] {
        let r = send(&client, base, Some(&owner), "GET", path, None).await;
        let value = body(&r);
        match path {
            "/api/v1/me" => assert!(!value.to_string().contains(&foreign_personal.to_string())),
            "/api/v1/me/summary" => {
                assert_eq!(value["totals"]["current"]["attempts"], "3");
                assert_eq!(value["totals"]["current"]["known_cost_microusd"], "70");
                assert!(!value["workspaces"].as_array().unwrap().is_empty());
                assert!(
                    value["workspaces"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|w| w["budgets"].as_array().is_some_and(|b| !b.is_empty()))
                );
            }
            "/api/v1/me/keys" => {
                // Six of the owner's own keys; never the member's.
                assert_eq!(value["data"].as_array().unwrap().len(), 6);
                assert!(
                    !value
                        .to_string()
                        .contains(foreign_key["id"].as_str().unwrap())
                );
                let by_status = |status: &str| {
                    value["data"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .filter(|k| k["status"] == status)
                        .count()
                };
                assert_eq!(
                    (
                        by_status("active"),
                        by_status("disabled"),
                        by_status("revoked")
                    ),
                    (4, 1, 1)
                );
            }
            "/api/v1/me/models" => {
                assert_eq!(value["models"].as_array().unwrap().len(), 1);
                assert_eq!(value["not_serving"].as_array().unwrap().len(), 1);
            }
            "/api/v1/me/notifications" | "/api/v1/me/sessions" => {
                assert!(!value["data"].as_array().unwrap().is_empty());
                if path.ends_with("notifications") {
                    first_event = value["data"][0]["id"].clone();
                }
            }
            "/api/v1/me/notifications/summary" => assert!(value["unread"].as_u64().unwrap() > 0),
            "/api/v1/me/export" => {
                assert_eq!(value["requests"]["items"].as_array().unwrap().len(), 3);
                assert!(value["requests"]["items"].to_string().contains("\"70\""));
                assert_eq!(value["files"]["items"].as_array().unwrap().len(), 2);
                assert_eq!(
                    value["platform_roles"]["items"].as_array().unwrap().len(),
                    2
                );
                assert_eq!(
                    r.headers["x-omg-export-sha256"].to_str().unwrap(),
                    hex::encode(Sha256::digest(&r.bytes))
                );
            }
            _ => unreachable!(),
        }
        assert!(
            !value
                .to_string()
                .contains(key_value["token"].as_str().unwrap())
        );
        observed.observe(&r);
    }
    // A plain member sees no workspace-wide budgets; an administrator whose
    // accounting drifted sees that window as unknown.
    let r = send(
        &client,
        base,
        Some(&second),
        "GET",
        "/api/v1/me/summary",
        None,
    )
    .await;
    let member_summary = body(&r);
    assert!(member_summary["workspaces"][0]["budgets"].is_null());
    observed.observe(&r);
    let r = send(
        &client,
        base,
        Some(&admin),
        "GET",
        "/api/v1/me/summary",
        None,
    )
    .await;
    let admin_summary = body(&r);
    assert!(
        admin_summary
            .to_string()
            .contains("\"headroom_status\":\"unknown\"")
    );
    observed.observe(&r);
    // Admins and Auditors export too; the Admin's own audit trail has events with no resource.
    for who in [&admin, &requester] {
        let r = send(&client, base, Some(who), "GET", "/api/v1/me/export", None).await;
        assert_eq!(body(&r)["format"], "omg.subject-export.v1");
        observed.observe(&r);
    }
    // Both live redaction variants, independently of the generic feed shape.
    for (actor, redacted) in [(&admin, false), (&auditor, true)] {
        let r = send(
            &client,
            base,
            Some(actor),
            "GET",
            "/api/v1/me/notifications",
            None,
        )
        .await;
        let value = body(&r);
        let ceiling = value["data"]
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["kind"] == "admission_ceiling" && v["details"]["personal"] == false)
            .unwrap();
        if redacted {
            assert!(ceiling["workspace"].is_null());
            assert!(ceiling["details"].get("workspace_name").is_none());
        } else {
            assert_eq!(ceiling["workspace"]["id"], json!(f.team));
            assert_eq!(ceiling["details"]["workspace_name"], "Team");
        }
        let personal = value["data"]
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["kind"] == "admission_ceiling" && v["details"]["personal"] == true)
            .unwrap();
        assert!(personal["workspace"].is_null());
        assert!(
            personal["details"]
                .get("workspace_name")
                .is_none_or(Value::is_null)
        );
        assert!(!personal.to_string().contains(&f.personal.to_string()));
        // The provider failure names its connection to both platform readers.
        assert!(
            value["data"]
                .as_array()
                .unwrap()
                .iter()
                .any(|v| v["kind"] == "provider_failing"
                    && v["connection"]["id"] == json!(connection))
        );
        observed.observe(&r);
    }
    // Boundary vectors of the reads that take query parameters.
    query_vectors(
        &mut observed,
        "GET",
        "/api/v1/me/keys",
        &[("status", "active")],
        async || {
            (
                (),
                vec![
                    ("limit".to_owned(), "50".to_owned()),
                    ("offset".to_owned(), "0".to_owned()),
                ],
            )
        },
        async |_: &(), query| {
            Probe::from(
                send(
                    &client,
                    base,
                    Some(&owner),
                    "GET",
                    &target("/api/v1/me/keys", &query),
                    None,
                )
                .await,
            )
        },
    )
    .await;
    query_vectors_fixed(
        &mut observed,
        "GET",
        "/api/v1/me/models",
        &[("limit", "10")],
        async |query| {
            send(
                &client,
                base,
                Some(&owner),
                "GET",
                &target("/api/v1/me/models", &query),
                None,
            )
            .await
        },
    )
    .await;
    query_vectors(
        &mut observed,
        "GET",
        "/api/v1/me/notifications",
        &[],
        async || {
            (
                (),
                vec![
                    ("limit".to_owned(), "50".to_owned()),
                    ("offset".to_owned(), "0".to_owned()),
                    // The only member: carried by the base so a positive request has it.
                    ("status".to_owned(), "unread".to_owned()),
                ],
            )
        },
        async |_: &(), query| {
            Probe::from(
                send(
                    &client,
                    base,
                    Some(&owner),
                    "GET",
                    &target("/api/v1/me/notifications", &query),
                    None,
                )
                .await,
            )
        },
    )
    .await;
    query_vectors_fixed(
        &mut observed,
        "GET",
        "/api/v1/me/summary",
        &[("totals_only", "true")],
        async |query| {
            send(
                &client,
                base,
                Some(&owner),
                "GET",
                &target("/api/v1/me/summary", &query),
                None,
            )
            .await
        },
    )
    .await;
    // Each export is rate limited to three an hour per person: a fresh person per request.
    let today = chrono::Utc::now().date_naive();
    let (from, to) = (
        (today - chrono::Days::new(1)).to_string(),
        today.to_string(),
    );
    query_vectors(
        &mut observed,
        "GET",
        "/api/v1/me/export",
        &[],
        async || {
            let person = user(
                &pool,
                &format!("export-{}", Uuid::new_v4().simple()),
                "user",
            )
            .await;
            (
                browser(&pool, &person, false).await,
                vec![
                    ("from".to_owned(), from.clone()),
                    ("to".to_owned(), to.clone()),
                ],
            )
        },
        async |session: &Browser, query| {
            Probe::from(
                send(
                    &client,
                    base,
                    Some(session),
                    "GET",
                    &target("/api/v1/me/export", &query),
                    None,
                )
                .await,
            )
        },
    )
    .await;
    // The invitation token is single use: a fresh invitation per body-vector run.
    let invite = async || {
        let (status, invitation) = call(
            &f.s,
            &f.owner,
            "POST",
            &format!("/api/v1/workspaces/{}/invitations", f.project),
            json!({"email":f.outsider.email,"role":"member"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        invitation["token"].clone()
    };
    let token = invite().await;
    body_vectors_http(
        &mut observed,
        "POST",
        "/api/v1/invitations/accept",
        json!({"token":token}),
        async |request| {
            Probe::from(
                send(
                    &client,
                    base,
                    Some(&requester),
                    "POST",
                    "/api/v1/invitations/accept",
                    Some(request),
                )
                .await,
            )
        },
    )
    .await;
    let token = invite().await;
    body_vectors_http(
        &mut observed,
        "POST",
        "/api/v1/invitations/accept",
        json!([token]),
        async |request| {
            Probe::from(
                send(
                    &client,
                    base,
                    Some(&requester),
                    "POST",
                    "/api/v1/invitations/accept",
                    Some(request),
                )
                .await,
            )
        },
    )
    .await;
    // Positive control before the vectors (which also mark everything read):
    // marking really changes unread incidents.
    let r = send(
        &client,
        base,
        Some(&owner),
        "POST",
        "/api/v1/me/notifications/read",
        Some(json!({"all":true})),
    )
    .await;
    assert!(body(&r)["marked"].as_u64().unwrap() > 0);
    observed.observe(&r);
    // Marking read: everything, then one named incident, in both wire forms. The
    // seeds add the declared optional sibling the base omits, so the cross-field
    // rule (exactly one of `all: true` or `ids`) is exercised: `{"ids":[x],
    // "all":true}` and `[[x], true]` reach the handler, which refuses them.
    let event = json!([first_event]);
    for (valid, seeds) in [
        (json!({"ids":[first_event],"all":false}), vec![]),
        (json!({"ids":[first_event]}), vec![("/all", json!(true))]),
        (json!({"all":true}), vec![("/ids", event.clone())]),
        (json!([[first_event], false]), vec![]),
        (json!([[first_event]]), vec![("/1", json!(true))]),
        (json!([null, true]), vec![]),
    ] {
        body_vectors_seeded(
            &mut observed,
            "POST",
            "/api/v1/me/notifications/read",
            valid,
            &seeds,
            async |request| {
                Probe::from(
                    send(
                        &client,
                        base,
                        Some(&owner),
                        "POST",
                        "/api/v1/me/notifications/read",
                        Some(request),
                    )
                    .await,
                )
            },
        )
        .await;
    }
    let r = send(
        &client,
        base,
        Some(&owner),
        "DELETE",
        &format!("/api/v1/me/sessions/{}", other_session.id),
        None,
    )
    .await;
    assert_eq!(body(&r)["revoked"], 1);
    observed.observe(&r);
    let r = send(
        &client,
        base,
        Some(&owner),
        "POST",
        "/api/v1/me/sessions/revoke-others",
        None,
    )
    .await;
    assert!(body(&r)["revoked"].as_u64().unwrap() > 0);
    observed.observe(&r);
    let revoked: bool =
        sqlx::query_scalar("SELECT revoked_at IS NOT NULL FROM browser_sessions WHERE id=$1")
            .bind(keep_for_revoke_others.id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(revoked);
    // Refusals supplement existing matrices, and must not earn positive coverage.
    let r = send(&client, base, None, "GET", "/api/v1/me", None).await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    observed.observe(&r);
    let r = send(
        &client,
        base,
        Some(&owner),
        "GET",
        &format!("/api/v1/me/access-requests/{request_id}"),
        None,
    )
    .await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    observed.observe(&r);
    let r = send(&client, base, Some(&owner), "HEAD", "/api/v1/me", None).await;
    assert!(r.bytes.is_empty());
    observed.observe(&r);
    // Positional arrays are an existing serde wire form, not an API change.
    let (status, again) = call(
        &f.s,
        &f.owner,
        "POST",
        &format!("/api/v1/workspaces/{}/invitations", f.project),
        json!({"email":f.outsider.email,"role":"member"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let r = send(
        &client,
        base,
        Some(&requester),
        "POST",
        "/api/v1/invitations/accept",
        Some(json!([again["token"]])),
    )
    .await;
    assert_eq!(body(&r)["workspace_id"], json!(f.project));
    observed.observe(&r);
    let r = send(
        &client,
        base,
        Some(&owner),
        "POST",
        "/api/v1/me/notifications/read",
        Some(json!([null, true])),
    )
    .await;
    assert!(body(&r)["marked"].is_u64());
    observed.observe(&r);
    let r = send(
        &client,
        base,
        Some(&requester),
        "POST",
        "/api/v1/invitations/accept",
        Some(json!({"token":"a".repeat(2 * MIB)})),
    )
    .await;
    assert_eq!(r.status, StatusCode::PAYLOAD_TOO_LARGE);
    drop(servers);
    observed.finish(openapi::responses::IDENTITY_SELF_SERVICE_AREAS);
}
