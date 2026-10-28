//! Slice 6 executed, populated wire fixtures against a running loopback gateway.
use super::*;
use openapi::responses::{Captured, PositiveFixtures, capture_http};
use sha2::{Digest, Sha256};

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
fn multipart(fields: &[(&str, &str)], file: Option<&str>) -> Vec<u8> {
    let mut body = String::new();
    for (name, value) in fields {
        body.push_str(&format!(
            "--contract\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"
        ));
    }
    if let Some(file) = file {
        body.push_str(&format!("--contract\r\nContent-Disposition: form-data; name=\"file\"; filename=\"contract.txt\"\r\nContent-Type: text/plain\r\n\r\n{file}\r\n"));
    }
    body.push_str("--contract--\r\n");
    body.into_bytes()
}
struct Wire {
    client: reqwest::Client,
    base: String,
    observed: PositiveFixtures,
}
impl Wire {
    async fn send(
        &mut self,
        b: &Browser,
        method: &str,
        path: &str,
        body: Option<Value>,
    ) -> Captured {
        let mut r = self
            .client
            .request(method.parse().unwrap(), format!("{}{path}", self.base))
            .header("origin", ORIGIN)
            .header("cookie", format!("omg_session={}", b.token))
            .header("x-csrf-token", &b.csrf);
        if let Some(body) = body {
            r = r.json(&body);
        }
        let r = capture_http(method, path, r.send().await.unwrap()).await;
        self.observed.observe(&r);
        r
    }
    async fn json(&mut self, b: &Browser, method: &str, path: &str, body: Option<Value>) -> Value {
        let r = self.send(b, method, path, body).await;
        assert!(r.status.is_success(), "{method} {path}: {}", r.status);
        let v: Value = serde_json::from_slice(&r.bytes).unwrap();
        if let Some(data) = v.get("data") {
            assert!(
                !data.as_array().unwrap().is_empty(),
                "populated fixture: {path}"
            );
        }
        v
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
        assert_eq!(r.status, want, "{method} {path}");
    }
}
async fn execution(pool: &PgPool, ws: Uuid, key: Uuid, deployment: Uuid, redacted: bool) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO inference_executions(id,workspace_id,api_key_id,deployment_id,public_model,provider,streamed,state,root_request_id,input_tokens,output_tokens,completed_at,elapsed_ms,client_session_id,details_redacted_at) VALUES($1,$2,$3,$4,'contract-log','openai',false,'succeeded',$1,5,1,clock_timestamp()+interval '20 milliseconds',20,'contract-session',CASE WHEN $5 THEN now() END)")
        .bind(id).bind(ws).bind(key).bind(deployment).bind(redacted).execute(pool).await.unwrap();
    id
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
    let m = model(&pool, "contract-log").await;
    direct(&pool, f.team, m).await;
    direct(&pool, f.personal, m).await;
    let (connection, deployment) = (Uuid::new_v4(), Uuid::new_v4());
    sqlx::query("INSERT INTO provider_connections(id,name,provider,credential_ref,enabled) VALUES($1,'Contract connection','openai','env:UNUSED',true)").bind(connection).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO deployments(id,model_id,provider_connection_id,upstream_model,enabled) VALUES($1,$2,$3,'contract-upstream',true)").bind(deployment).bind(m).bind(connection).execute(&pool).await.unwrap();
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
        axum::serve(listener, app).await.unwrap()
    }));
    let mut w = Wire {
        client: reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap(),
        base,
        observed: PositiveFixtures::default(),
    };
    let ws = format!("/api/v1/workspaces/{}", f.team);
    let personal = format!("/api/v1/workspaces/{}", f.personal);

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
            Some(json!({"name":"Service key","expires_in_days":30,"service_account_id":account})),
        )
        .await);
    let root = execution(&pool, f.team, own, deployment, false).await;
    let redacted = execution(&pool, f.team, own, deployment, true).await;
    let member_root = execution(&pool, f.team, member_key, deployment, false).await;
    let private_root = execution(&pool, f.personal, private_key, deployment, false).await;
    sqlx::query(
        "UPDATE api_keys SET expires_at=NULL,created_at=now()-interval '200 days' WHERE id=$1",
    )
    .bind(member_key)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO policy_budgets(layer,kind,period,amount_microusd) VALUES('type','team','month',9007199254740995) ON CONFLICT DO NOTHING").execute(&pool).await.unwrap();
    let keypath = format!("{ws}/keys/{own}");
    w.json(&owner,"PUT",&format!("{keypath}/policy"),Some(json!({"requests_per_minute":null,"tokens_per_minute":null,"concurrent_requests":null,"budgets":[{"period":"month","amount_microusd":"9007199254740993"}]}))).await;
    for suffix in ["", "/access", "/policy", "/stats"] {
        let v = w
            .json(&owner, "GET", &format!("{keypath}{suffix}"), None)
            .await;
        assert!(v.get("token").is_none() && v.get("secret_hash").is_none());
    }
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
    }
    assert_eq!(
        w.json(&owner, "GET", &format!("{ws}/keys/{service_key}"), None)
            .await["status"],
        "revoked"
    );
    let all = w
        .json(&owner, "GET", &format!("{ws}/key-issuance"), None)
        .await;
    assert!(all.get("workspace").is_some());
    assert!(
        w.json(&member, "GET", &format!("{ws}/key-issuance"), None)
            .await
            .get("workspace")
            .is_none()
    );
    w.json(&member, "GET", &format!("{ws}/key-safety"), None)
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

    // Every log form has nonempty persisted attempts, including owner-private
    // and details-redacted variants. No paid upstream runs in this fixture.
    for suffix in ["/executions", "/generations", "/requests", "/logs/metrics"] {
        w.json(&owner, "GET", &format!("{ws}{suffix}"), None).await;
    }
    let own_member_logs = w
        .json(&member, "GET", &format!("{ws}/requests"), None)
        .await;
    assert_eq!(own_member_logs["data"].as_array().unwrap().len(), 1);
    assert_eq!(
        own_member_logs["data"][0]["root_request_id"],
        json!(member_root)
    );
    for r in [root, redacted] {
        let v = w
            .json(&owner, "GET", &format!("{ws}/requests/{r}"), None)
            .await;
        assert!(v.get("prompt").is_none() && v.get("response").is_none());
        assert!(
            v["cost_microusd"].is_null(),
            "missing reservation is unknown, not zero"
        );
        assert_eq!(v["attempts"][0]["unresolved_reason"], "missing_reservation");
        assert_eq!(
            v["attempts"][0]["details_redacted_at"].is_string(),
            r == redacted
        );
    }
    let private = w
        .json(
            &owner,
            "GET",
            &format!("{personal}/requests/{private_root}"),
            None,
        )
        .await;
    assert!(private["trace_id"].is_null());
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
        &format!("{ws}/requests/{root}"),
        None,
        StatusCode::NOT_FOUND,
    )
    .await;
    for suffix in ["generations", "metrics", "requests", "sessions"] {
        let v = w
            .json(
                &auditor,
                "GET",
                &format!("/api/v1/platform/logs/{suffix}"),
                None,
            )
            .await;
        assert!(!v.to_string().contains(&private_root.to_string()));
    }
    w.json(
        &auditor,
        "GET",
        &format!("/api/v1/platform/logs/requests/{root}"),
        None,
    )
    .await;
    w.json(
        &auditor,
        "GET",
        &format!("/api/v1/platform/logs/sessions/{}/contract-session", f.team),
        None,
    )
    .await;

    // Multipart upload and opaque binary download; raw wire capture precedes
    // decoding. Visibility widens independently of delete permission.
    let uploadpath = format!("{ws}/files/upload");
    let bytes = multipart(
        &[
            ("purpose", "user_data"),
            ("expires_after[anchor]", "created_at"),
            ("expires_after[seconds]", "3600"),
        ],
        Some("opaque contents"),
    );
    let r = w
        .client
        .post(format!("{}{uploadpath}", w.base))
        .header("origin", ORIGIN)
        .header("cookie", format!("omg_session={}", owner.token))
        .header("x-csrf-token", &owner.csrf)
        .header("content-type", "multipart/form-data; boundary=contract")
        .body(bytes)
        .send()
        .await
        .unwrap();
    let r = capture_http("POST", &uploadpath, r).await;
    assert_eq!(r.status, StatusCode::OK);
    w.observed.observe(&r);
    let file: Value = serde_json::from_slice(&r.bytes).unwrap();
    let fileid = file["id"].as_str().unwrap();
    w.json(&owner, "GET", &format!("{ws}/files"), None).await;
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

    // A local gateway-mode batch can be cancelled without touching a provider.
    let job = Uuid::new_v4();
    let stored = crate::filestore::files::parse_public_id(fileid).unwrap();
    sqlx::query("INSERT INTO async_jobs(id,kind,workspace_id,api_key_id,deployment_id,execution_id,public_model,provider,poll_deadline_at,batch_endpoint,batch_mode,user_id,input_file_id,work_file_id,request_total,request_completed,request_failed) VALUES($1,'batch',$2,$3,$4,$5,'contract-log','openai',now()+interval '1 hour','/v1/chat/completions','gateway',$6,$7,$7,1,0,0)")
        .bind(job).bind(f.team).bind(own).bind(deployment).bind(root).bind(f.owner.user_id).bind(stored).execute(&pool).await.unwrap();
    let batchpath = format!("{ws}/batches/batch_{}", job.simple());
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
    w.json(&owner, "GET", &batchpath, None).await;
    let job_request = w
        .json(&owner, "GET", &format!("{ws}/requests/{root}"), None)
        .await;
    assert_eq!(job_request["job"]["kind"], "batch");
    assert_eq!(job_request["job"]["request_counts"]["total"], 1);
    let jobs = w.json(&owner, "GET", &format!("{ws}/requests"), None).await;
    assert!(
        jobs["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["job"]["kind"] == "batch")
    );
    w.json(&auditor, "GET", "/api/v1/platform/batches", None)
        .await;
    w.json(&owner, "POST", &format!("{batchpath}/cancel"), None)
        .await;
    w.json(&owner, "DELETE", &format!("{ws}/files/{fileid}"), None)
        .await;

    let hold = id(&w
        .json(
            &admin,
            "POST",
            "/api/v1/platform/legal-holds",
            Some(json!({"workspace_id":f.team,"reference":"CONTRACT-1"})),
        )
        .await);
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
    let token=w.json(&admin,"POST","/api/v1/platform/admin-tokens",Some(json!({"name":"Contract management","scopes":["platform:read"],"expires_in_days":30}))).await;
    let tokenid = id(&token);
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
                .unwrap(),
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
    w.json(&owner, "GET", &format!("{ws}/audit"), None).await;
    w.json(&auditor, "GET", "/api/v1/platform/audit", None)
        .await;
    // Actual issuance refusal, with no minted credential and a bounded wait.
    sqlx::query("INSERT INTO workspace_local_policies(workspace_id,key_issuances_per_holder) VALUES($1,1) ON CONFLICT(workspace_id) DO UPDATE SET key_issuances_per_holder=1").bind(f.team).execute(&pool).await.unwrap();
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
    // Multipart has no serde DTO: exercise the actual streamed parser after
    // the successful upload, preserving valid fields for unknown-field probes.
    for (fields, file) in [
        (
            vec![
                ("purpose", "user_data"),
                ("unexpected_configuration", "true"),
            ],
            Some("content"),
        ),
        (vec![], Some("content")),
        (vec![("purpose", "user_data")], None),
        (vec![("purpose", "user_data")], Some("")),
        (
            vec![("purpose", "user_data"), ("purpose", "user_data")],
            Some("content"),
        ),
        (
            vec![("purpose", "user_data"), ("expires_after[seconds]", "null")],
            Some("content"),
        ),
    ] {
        let r = w
            .client
            .post(format!("{}{uploadpath}", w.base))
            .header("origin", ORIGIN)
            .header("cookie", format!("omg_session={}", owner.token))
            .header("x-csrf-token", &owner.csrf)
            .header("content-type", "multipart/form-data; boundary=contract")
            .body(multipart(&fields, file))
            .send()
            .await
            .unwrap();
        let r = capture_http("POST", &uploadpath, r).await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST);
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
    w.observed.finish(openapi::responses::SLICE6_AREAS);
}
