//! Real loopback gateway, browser authentication and runtime-role PostgreSQL.
use super::*;
use crate::timestamp::TimestampFormat as _;
use sqlx::postgres::PgPoolOptions;
use std::sync::Arc;

struct Server(tokio::task::JoinHandle<()>);
impl Drop for Server {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[dbtest::test]
async fn free_price_runtime_http_operator_flow(pool: PgPool) {
    let f = crate::governance::tests::db::fixture(pool.clone()).await;
    let free = f.price(0).await;
    let start = f.start();
    crate::governance::admit(
        &f.store,
        &start,
        &crate::governance::tests::db::request(),
        30,
    )
    .await
    .unwrap();
    crate::governance::finish(
        &f.store,
        &crate::governance::tests::db::done(start.id, None, None),
    )
    .await
    .unwrap();
    let principal = |id, email: &str| BrowserPrincipal {
        user_id: id,
        email: email.into(),
        platform_admin: false,
        platform_auditor: false,
    };
    let admin = session(&pool, &principal(f.owner, "owner@test.invalid")).await;
    let member = session(&pool, &principal(f.other, "member@test.invalid")).await;
    // Fresh test clusters need the restricted role before the allowlist can be
    // applied; an existing role is left unchanged and checked after SET ROLE.
    sqlx::raw_sql("DO $$ BEGIN IF NOT EXISTS(SELECT FROM pg_roles WHERE rolname='gateway_runtime') THEN CREATE ROLE gateway_runtime NOLOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION NOBYPASSRLS; END IF; END $$;")
        .execute(&pool).await.unwrap();
    f.store.apply_runtime_grants().await.unwrap();
    let runtime = PgPoolOptions::new()
        .max_connections(5)
        .after_connect(|c, _| {
            Box::pin(async move {
                sqlx::Executor::execute(c, "SET ROLE gateway_runtime; SET omg.scope_lock_audit=on")
                    .await?;
                Ok(())
            })
        })
        .connect_with((*pool.connect_options()).clone())
        .await
        .unwrap();
    let role: String = sqlx::query_scalar("SELECT current_user")
        .fetch_one(&runtime)
        .await
        .unwrap();
    assert_eq!(role, "gateway_runtime");
    let restricted: bool = sqlx::query_scalar("SELECT NOT (rolsuper OR rolcreatedb OR rolcreaterole OR rolreplication OR rolbypassrls) FROM pg_roles WHERE rolname=current_user")
        .fetch_one(&runtime).await.unwrap();
    assert!(
        restricted,
        "HTTP fixture requires an unprivileged runtime role"
    );
    let store = Store::new(runtime.clone());
    let mock = MockProvider::start(None).await;
    let identity = mock.state(runtime).await;
    let engine = crate::inference::Engine::new(
        Arc::new(store.clone()),
        crate::providers::ProviderRegistry::default(),
        crate::inference::EngineLimits::default(),
    )
    .unwrap();
    let router = crate::http::router_with_identity(store, None, engine, identity);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let _server = Server(tokio::spawn(async move {
        axum::serve(
            listener,
            router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .unwrap();
    }));
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .unwrap();
    let path = format!("{base}/api/v1/platform/reservations/resolve-free-price");
    let send = |who: Option<&Session>, csrf: bool, origin: &str, body: Value| {
        let mut r = client.post(&path).header("origin", origin).json(&body);
        if let Some(who) = who {
            r = r.header("cookie", format!("omg_session={}", who.cookie));
            if csrf {
                r = r.header("x-csrf-token", &who.csrf);
            }
        }
        async move { r.send().await.unwrap() }
    };
    assert_eq!(
        send(None, true, ORIGIN, json!({})).await.status().as_u16(),
        401
    );
    assert_eq!(
        send(Some(&member), true, ORIGIN, json!({}))
            .await
            .status()
            .as_u16(),
        403
    );
    assert_eq!(
        send(Some(&admin), false, ORIGIN, json!({}))
            .await
            .status()
            .as_u16(),
        403
    );
    assert_eq!(
        send(Some(&admin), true, "http://wrong.invalid", json!({}))
            .await
            .status()
            .as_u16(),
        403
    );
    sqlx::query(
        "INSERT INTO platform_role_grants(user_id,role,source) VALUES($1,'admin','manual')",
    )
    .bind(f.other)
    .execute(&pool)
    .await
    .unwrap();
    let mut seen = crate::management::openapi::responses::PositiveFixtures::default();
    let preview = send(Some(&member), true, ORIGIN, json!({})).await;
    assert_eq!(preview.status().as_u16(), 200);
    let preview = crate::management::openapi::responses::capture_http(
        "POST",
        "/api/v1/platform/reservations/resolve-free-price",
        preview,
    )
    .await;
    assert_eq!(
        serde_json::from_slice::<Value>(&preview.bytes).unwrap(),
        json!({"eligible_count":"1","resolved_count":"0","next_offset":null})
    );
    seen.observe(&preview);
    let body = json!({"apply":true,"expected_count":"1","reason":"loopback operator proof"});
    let applied = send(Some(&member), true, ORIGIN, body.clone()).await;
    assert_eq!(applied.status().as_u16(), 200);
    let applied = crate::management::openapi::responses::capture_http(
        "POST",
        "/api/v1/platform/reservations/resolve-free-price",
        applied,
    )
    .await;
    assert_eq!(
        serde_json::from_slice::<Value>(&applied.bytes).unwrap()["resolved_count"],
        "1"
    );
    seen.observe(&applied);
    assert_eq!(
        send(Some(&admin), true, ORIGIN, body)
            .await
            .status()
            .as_u16(),
        409
    );
    let row:(Option<i64>,Option<i64>,Option<i64>)=sqlx::query_as("SELECT actual_microusd,input_tokens,output_tokens FROM governance_reservations WHERE execution_id=$1").bind(start.id).fetch_one(&pool).await.unwrap();
    assert_eq!(row, (Some(0), None, None));
    let exported = client
        .get(format!("{base}/api/v1/me/export"))
        .header("cookie", format!("omg_session={}", member.cookie))
        .send()
        .await
        .unwrap();
    assert_eq!(exported.status().as_u16(), 200);
    let exported = exported.text().await.unwrap();
    assert!(
        !exported.contains(&start.id.to_string()),
        "operator export exposed private execution identity"
    );
    assert!(
        !exported.contains(&f.principal.key_id.to_string()),
        "operator export exposed private key identity"
    );
    let valid = json!({"apply":false,"month":gateway_datetime_compat::UtcInstant::now().format("%Y-%m").to_string(),"workspace_id":f.principal.workspace_id,"deployment_id":f.deployment,"expected_count":"0","reason":"operator reference","offset":"0"});
    let positional = json!([
        false,
        valid["month"],
        valid["workspace_id"],
        valid["deployment_id"],
        "0",
        "operator reference",
        "0"
    ]);
    let mut legacy = positional.as_array().unwrap().clone();
    legacy.pop();
    let response = send(Some(&admin), true, ORIGIN, json!(legacy)).await;
    assert_eq!(response.status().as_u16(), 200);
    seen.observe(
        &crate::management::openapi::responses::capture_http(
            "POST",
            "/api/v1/platform/reservations/resolve-free-price",
            response,
        )
        .await,
    );
    for valid in [valid, positional] {
        let (month, count, offset) = if valid.is_array() {
            ("/1", "/4", "/6")
        } else {
            ("/month", "/expected_count", "/offset")
        };
        let candidates = vec![
            (month, json!("2026-01")),
            (month, json!("2026-1")),
            (month, json!("2026-010")),
            (count, json!("0")),
            (count, json!("")),
            (count, json!("10000")),
            (count, json!("100000")),
            (offset, json!("0")),
            (offset, json!("10000")),
            (offset, json!("00")),
            (offset, json!("-1")),
            (offset, json!("9223372036854765807")),
            (offset, json!("9223372036854765808")),
            (offset, json!("9223372036854775807")),
            (offset, json!("10000000000000000000")),
        ];
        crate::management::openapi::http_vectors::body_vectors_extended(
            &mut seen,
            "POST",
            "/api/v1/platform/reservations/resolve-free-price",
            valid,
            &[],
            &candidates,
            async |body| {
                crate::management::openapi::responses::capture_http(
                    "POST",
                    "/api/v1/platform/reservations/resolve-free-price",
                    send(Some(&admin), true, ORIGIN, body).await,
                )
                .await
                .into()
            },
        )
        .await;
    }
    sqlx::query("WITH ids AS MATERIALIZED(SELECT gen_random_uuid() AS id FROM generate_series(1,10001)), e AS (INSERT INTO inference_executions(id,workspace_id,api_key_id,deployment_id,public_model,provider,streamed,state,root_request_id) SELECT id,$1,$2,$3,'company/smart','openai',true,'failed',id FROM ids RETURNING id,started_at) INSERT INTO governance_reservations(execution_id,workspace_id,api_key_id,deployment_id,price_id,admitted_at,minute_start,month_start,lease_expires_at,state,held_microusd) SELECT id,$1,$2,$3,$4,started_at,date_trunc('minute',started_at,'UTC'),date_trunc('month',started_at,'UTC'),started_at,'unknown',0 FROM e")
        .bind(f.principal.workspace_id).bind(f.principal.key_id).bind(f.deployment).bind(free).execute(&pool).await.unwrap();
    for (offset, count, next) in [("0", "10000", json!("10000")), ("10000", "1", Value::Null)] {
        let page = send(Some(&admin), true, ORIGIN, json!({"offset":offset})).await;
        assert_eq!(page.status().as_u16(), 200);
        let page = crate::management::openapi::responses::capture_http(
            "POST",
            "/api/v1/platform/reservations/resolve-free-price",
            page,
        )
        .await;
        assert_eq!(
            serde_json::from_slice::<Value>(&page.bytes).unwrap(),
            json!({"eligible_count":count,"resolved_count":"0","next_offset":next})
        );
        seen.observe(&page);
    }
    super::batch_envelope_http_tests::run(&f, &client, &base, &admin, &member, &mut seen).await;
    seen.finish(crate::management::openapi::responses::FREE_PRICE_AREAS);
    let mut tx = crate::db::begin(&pool).await.unwrap();
    crate::governance::locks::exclusive(&mut tx, [crate::governance::locks::Scope::User(f.owner)])
        .await
        .unwrap();
    let token = crate::admin_tokens::create_admin_token(
        &mut tx,
        f.owner,
        "route refusal",
        &["governance:write".into()],
        1,
        None,
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(
        client
            .post(&path)
            .bearer_auth(&token.token)
            .json(&json!({}))
            .send()
            .await
            .unwrap()
            .status()
            .as_u16(),
        403
    );
    let inference = crate::auth::NewApiKey::generate().unwrap();
    sqlx::query("INSERT INTO api_keys(id,workspace_id,issued_to_user_id,name,secret_hash) VALUES($1,$2,$3,'route refusal',$4)").bind(inference.id).bind(f.principal.workspace_id).bind(f.owner).bind(inference.digest.as_slice()).execute(&pool).await.unwrap();
    assert_eq!(
        client
            .post(&path)
            .bearer_auth(&inference.token)
            .json(&json!({}))
            .send()
            .await
            .unwrap()
            .status()
            .as_u16(),
        401
    );
    let batch_path = format!("{base}/api/v1/platform/reservations/resolve-batch-envelopes");
    for (credential, status) in [(&token.token, 403), (&inference.token, 401)] {
        assert_eq!(
            client
                .post(&batch_path)
                .bearer_auth(credential)
                .json(&json!({}))
                .send()
                .await
                .unwrap()
                .status()
                .as_u16(),
            status
        );
    }
}
