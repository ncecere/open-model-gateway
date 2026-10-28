//! Executed coverage, not a list of claimed fixture names. Every observation
//! comes from a real handler; this suite also runs the gateway over loopback.
use super::*;
use openapi::responses::{Captured, PositiveFixtures, capture_http};
use sha2::{Digest, Sha256};

#[path = "setup_openapi_fixture_tests.rs"]
mod setup;

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
    id: Uuid,
}
async fn browser(pool: &PgPool, user: &BrowserPrincipal) -> Browser {
    let token = crate::entropy::hex_token().unwrap();
    let csrf = crate::entropy::hex_token().unwrap();
    let id = sqlx::query_scalar("INSERT INTO browser_sessions(token_hash,user_id,csrf_hash,expires_at,verified_email,last_seen_at) VALUES($1,$2,$3,now()+interval '1 hour',$4,now()) RETURNING id")
        .bind(Sha256::digest(token.as_bytes()).as_slice()).bind(user.user_id)
        .bind(Sha256::digest(csrf.as_bytes()).as_slice()).bind(&user.email).fetch_one(pool).await.unwrap();
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
fn body(captured: &Captured) -> Value {
    assert_eq!(captured.status, StatusCode::OK);
    serde_json::from_slice(&captured.bytes).unwrap()
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn management_openapi_completed_area_fixtures(pool: PgPool) {
    let mut observed = PositiveFixtures::default();
    crate::identity::tests::management_openapi_auth_fixtures(&pool, &mut observed).await;
    let f = fixture(&pool).await;
    branding_tests::management_openapi_branding_fixtures(&f, &mut observed).await;
    let owner = browser(&pool, &f.owner).await;
    let requester = browser(&pool, &f.outsider).await;
    let admin = browser(&pool, &f.admin).await;
    let auditor = browser(&pool, &f.auditor).await;
    let other_session = browser(&pool, &f.owner).await;
    let keep_for_revoke_others = browser(&pool, &f.owner).await;
    let foreign_personal =
        workspace(&pool, "Foreign private", "personal", f.outsider.user_id).await;
    sqlx::query("UPDATE workspaces SET discoverable=true WHERE id=$1")
        .bind(f.team)
        .execute(&pool)
        .await
        .unwrap();
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
    // Real persisted, nonempty request/export records with unknown cost.
    let execution = Uuid::new_v4();
    sqlx::query("INSERT INTO inference_executions(id,workspace_id,api_key_id,deployment_id,public_model,provider,streamed,state,root_request_id,input_tokens,output_tokens) VALUES($1,$2,$3,$4,'contract-serving','openai',false,'succeeded',$1,5,1)").bind(execution).bind(f.team).bind(own_key).bind(deployment).execute(&pool).await.unwrap();
    let (status, _) = call(&f.s, &f.admin, "PUT", "/api/v1/platform/workspace-types/team/policy", json!({"requests_per_minute":null,"tokens_per_minute":null,"concurrent_requests":null,"budgets":[{"period":"month","amount_microusd":"9007199254740993"}]})).await;
    assert_eq!(status, StatusCode::OK);
    sqlx::query("UPDATE api_keys SET expires_at=now()+interval '3 days'")
        .execute(&pool)
        .await
        .unwrap();
    crate::alerts::evaluate_once(&f.s).await.unwrap().unwrap();
    let (status, ceiling) = call(&f.s, &f.admin, "POST", "/api/v1/platform/alerts/rules", json!({"name":"Contract ceiling","kind":"admission_ceiling","window_minutes":5,"ceiling_requests_per_second":200,"ceiling_lock_wait_ms":250,"notify_platform_admins":true})).await;
    assert_eq!(status, StatusCode::OK);
    sqlx::query("INSERT INTO alert_events(id,rule_id,kind,subject_key,level,severity,workspace_id,summary,details) VALUES($1,$2,'admission_ceiling',$3,1,'warning',$4,'A workspace is near its admission ceiling',jsonb_build_object('scope','workspace','personal',false,'workspace_name','Team','workspace_kind','team','window_minutes',5,'admissions',60000,'admissions_per_second','200.0','by_rate',true,'by_lock_wait',false,'threshold_requests_per_second',200,'lock_wait_p95_at_least_ms',NULL,'threshold_lock_wait_ms',250))")
        .bind(Uuid::new_v4()).bind(id(&ceiling)).bind(format!("workspace:{}",f.team)).bind(f.team).execute(&pool).await.unwrap();
    // alerts::admission_ceiling_conditions suppresses personal references even
    // for Admins: the stored event deliberately has no workspace_id/name.
    sqlx::query("INSERT INTO alert_events(id,rule_id,kind,subject_key,level,severity,summary,details) VALUES($1,$2,'admission_ceiling','contract-personal',1,'warning','An API key in a personal workspace is near its admission ceiling',jsonb_build_object('scope','key','personal',true,'workspace_name',NULL,'workspace_kind','personal','window_minutes',5,'admissions',60000,'admissions_per_second','200.0','by_rate',true,'by_lock_wait',false,'threshold_requests_per_second',200,'lock_wait_p95_at_least_ms',NULL,'threshold_lock_wait_ms',250))")
        .bind(Uuid::new_v4()).bind(id(&ceiling)).execute(&pool).await.unwrap();
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

    let r = send(
        &client,
        &base,
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
    let submission =
        json!({"workspace_id":f.team,"reason":"Join team","idempotency_key":"contract-positive"});
    let r = send(
        &client,
        &base,
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
        let r = send(&client, &base, Some(&requester), "GET", &path, None).await;
        let value = body(&r);
        assert!(!value.to_string().contains("requester_email"));
        if let Some(data) = value.get("data") {
            assert!(!data.as_array().unwrap().is_empty());
        }
        observed.observe(&r);
    }
    let r = send(
        &client,
        &base,
        Some(&requester),
        "POST",
        &format!("/api/v1/me/access-requests/{request_id}/withdraw"),
        None,
    )
    .await;
    assert_eq!(body(&r)["status"], "withdrawn");
    observed.observe(&r);
    let r = send(
        &client,
        &base,
        Some(&requester),
        "POST",
        "/api/v1/invitations/accept",
        Some(json!({"token":invitation["token"]})),
    )
    .await;
    assert_eq!(body(&r)["workspace_id"], json!(f.project));
    observed.observe(&r);

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
        let r = send(&client, &base, Some(&owner), "GET", path, None).await;
        let value = body(&r);
        match path {
            "/api/v1/me" => assert!(!value.to_string().contains(&foreign_personal.to_string())),
            "/api/v1/me/summary" => {
                assert_eq!(value["totals"]["current"]["attempts"], "1");
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
                assert_eq!(value["data"].as_array().unwrap().len(), 1);
                assert!(
                    !value
                        .to_string()
                        .contains(foreign_key["id"].as_str().unwrap())
                );
            }
            "/api/v1/me/models" => {
                assert_eq!(value["models"].as_array().unwrap().len(), 1);
                assert_eq!(value["not_serving"].as_array().unwrap().len(), 1);
            }
            "/api/v1/me/notifications" | "/api/v1/me/sessions" => {
                assert!(!value["data"].as_array().unwrap().is_empty())
            }
            "/api/v1/me/notifications/summary" => assert!(value["unread"].as_u64().unwrap() > 0),
            "/api/v1/me/export" => {
                assert_eq!(value["requests"]["items"].as_array().unwrap().len(), 1);
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
    // Both live redaction variants, independently of the generic feed shape.
    for (actor, redacted) in [(&admin, false), (&auditor, true)] {
        let r = send(
            &client,
            &base,
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
        observed.observe(&r);
    }
    let r = send(
        &client,
        &base,
        Some(&owner),
        "POST",
        "/api/v1/me/notifications/read",
        Some(json!({"all":true})),
    )
    .await;
    assert!(body(&r)["marked"].as_u64().unwrap() > 0);
    observed.observe(&r);
    let r = send(
        &client,
        &base,
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
        &base,
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
    let r = send(&client, &base, None, "GET", "/api/v1/me", None).await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    observed.observe(&r);
    let r = send(
        &client,
        &base,
        Some(&owner),
        "GET",
        &format!("/api/v1/me/access-requests/{request_id}"),
        None,
    )
    .await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    observed.observe(&r);
    let r = send(&client, &base, Some(&owner), "HEAD", "/api/v1/me", None).await;
    assert!(r.bytes.is_empty());
    observed.observe(&r);
    // Real extractor probes AFTER positive controls. Null/omission acceptance
    // (which may still fail semantic checks) is compared to serde in unit vectors.
    for (path, valid) in [
        (
            "/api/v1/invitations/accept",
            json!({"token":invitation["token"]}),
        ),
        ("/api/v1/me/access-requests", submission),
        ("/api/v1/me/notifications/read", json!({"all":true})),
    ] {
        let mut unknown = valid.clone();
        unknown["unexpected_configuration"] = json!(true);
        for bad in [
            unknown,
            json!({"nested":{"unexpected_configuration":true}}),
            json!({}),
            json!([]),
            Value::Null,
        ] {
            let r = send(&client, &base, Some(&requester), "POST", path, Some(bad)).await;
            // {} deserializes for ReadInput, then fails its exactly-one semantic rule.
            assert!(matches!(
                r.status,
                StatusCode::BAD_REQUEST | StatusCode::UNPROCESSABLE_ENTITY
            ));
        }
    }
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
        &base,
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
        &base,
        Some(&requester),
        "POST",
        "/api/v1/me/access-requests",
        Some(json!([f.team, null, "contract-positional"])),
    )
    .await;
    assert_eq!(body(&r)["status"], "pending");
    observed.observe(&r);
    let r = send(
        &client,
        &base,
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
        &base,
        Some(&requester),
        "POST",
        "/api/v1/invitations/accept",
        Some(json!({"token":"a".repeat(2*1024*1024)})),
    )
    .await;
    assert_eq!(r.status, StatusCode::PAYLOAD_TOO_LARGE);
    observed.finish(openapi::responses::IDENTITY_SELF_SERVICE_AREAS);
}
