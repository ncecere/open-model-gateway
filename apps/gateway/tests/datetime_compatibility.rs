//! Black-box date/time contracts: real serve process, fresh PostgreSQL, loopback OIDC.
#![cfg(feature = "integration-tests")]
// Acceptance clients deliberately reach only their loopback gateway and issuer.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use axum::{
    Json, Router,
    routing::{get, post},
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use open_model_gateway::{bootstrap, config::Environment, store::Store};
use openidconnect::{
    PrivateSigningKey,
    core::{CoreEdDsaPrivateSigningKey, CoreJwsSigningAlgorithm},
};
use reqwest::{Client, Response, StatusCode};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::{ConnectOptions, Connection, PgConnection, PgPool};
use std::{
    process::Stdio,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use uuid::Uuid;

// Public upstream test key, identical to the existing unit-test issuer's key.
const TEST_KEY: &str = "-----BEGIN PRIVATE KEY-----\nMC4CAQAwBQYDK2VwBCIEICWeYPLxoZKHZlQ6rkBi11E9JwchynXtljATLqym/XS9\n-----END PRIVATE KEY-----";
fn signing_key() -> CoreEdDsaPrivateSigningKey {
    CoreEdDsaPrivateSigningKey::from_ed25519_pem(TEST_KEY, None).unwrap()
}

struct Issuer {
    base: String,
    claims: Arc<Mutex<Value>>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Issuer {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Issuer {
    async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let metadata = json!({
            "issuer":base,"authorization_endpoint":format!("{base}/authorize"),
            "token_endpoint":format!("{base}/token"),"jwks_uri":format!("{base}/jwks"),
            "response_types_supported":["code"],"subject_types_supported":["public"],
            "id_token_signing_alg_values_supported":["EdDSA"],"code_challenge_methods_supported":["S256"]
        });
        let claims = Arc::new(Mutex::new(Value::Null));
        let token_claims = claims.clone();
        let router = Router::new()
            .route(
                "/.well-known/openid-configuration",
                get(move || {
                    let metadata = metadata.clone();
                    async move { Json(metadata) }
                }),
            )
            .route(
                "/jwks",
                get(|| async { Json(json!({"keys":[signing_key().as_verification_key()]})) }),
            )
            .route(
                "/token",
                post(move || {
                    let claims = token_claims.lock().unwrap().clone();
                    async move {
                        // Sign raw JSON so malformed/out-of-range NumericDate claims reach
                        // the gateway's decoder; no SDK date conversion acts as the oracle.
                        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"EdDSA","typ":"JWT"}"#);
                        let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap());
                        let input = format!("{header}.{payload}");
                        let signature = signing_key()
                            .sign(&CoreJwsSigningAlgorithm::EdDsa, input.as_bytes())
                            .unwrap();
                        Json(json!({"access_token":"unused","token_type":"Bearer",
                        "id_token":format!("{input}.{}",URL_SAFE_NO_PAD.encode(signature))}))
                    }
                }),
            );
        let task = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        Self { base, claims, task }
    }
}

struct Gateway {
    client: Client,
    base: String,
    child: tokio::process::Child,
}
impl Gateway {
    async fn start(pool: &PgPool, issuer: &Issuer) -> Self {
        // Same grants as production, in the sqlx test's freshly migrated database.
        sqlx::raw_sql(include_str!("common/ensure_runtime_role.sql"))
            .execute(pool)
            .await
            .unwrap();
        assert!(sqlx::query_scalar::<_,bool>("SELECT NOT rolsuper AND NOT rolcreatedb AND NOT rolcreaterole AND NOT rolreplication AND NOT rolbypassrls FROM pg_roles WHERE rolname='gateway_runtime'").fetch_one(pool).await.unwrap());
        Store::new(pool.clone())
            .apply_runtime_grants()
            .await
            .unwrap();
        // The child reparses its URL, so the startup role must be encoded in it.
        let mut database = pool.connect_options().as_ref().to_url_lossy();
        database
            .query_pairs_mut()
            .append_pair("options", "-c role=gateway_runtime");
        let database = database.to_string();
        let mut runtime = PgConnection::connect(&database).await.unwrap();
        let (role, restricted): (String, bool) = sqlx::query_as(
            "SELECT current_user::text, NOT rolsuper AND NOT rolcreatedb AND NOT rolcreaterole AND NOT rolreplication AND NOT rolbypassrls FROM pg_roles WHERE rolname=current_user",
        )
        .fetch_one(&mut runtime)
        .await
        .unwrap();
        assert_eq!(role, "gateway_runtime", "gateway connection identity");
        assert!(
            restricted,
            "gateway connection has elevated role attributes"
        );
        runtime.close().await.unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let base = format!("http://{addr}");
        let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_open-model-gateway"));
        command
            .arg("serve")
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap())
            .env("DATABASE_URL", database)
            .env("GATEWAY_ENV_FILE", "/dev/null")
            .env("GATEWAY_ENV", "development")
            .env("GATEWAY_LISTEN", addr.to_string())
            .env("GATEWAY_PUBLIC_URL", &base)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        command
            .env("GATEWAY_OIDC_ISSUER", &issuer.base)
            .env("GATEWAY_OIDC_CLIENT_ID", "date-contract")
            .env("GATEWAY_LOGIN_RATE_PER_MINUTE", "10000");
        let child = command.spawn().unwrap();
        let client = Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(15))
            .build()
            .unwrap();
        let mut gateway = Self {
            client,
            base,
            child,
        };
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                assert!(
                    gateway.child.try_wait().unwrap().is_none(),
                    "gateway exited during startup"
                );
                if gateway
                    .client
                    .get(format!("{}/health/ready", gateway.base))
                    .send()
                    .await
                    .is_ok_and(|r| r.status() == StatusCode::OK)
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("gateway readiness");
        gateway
    }
    async fn get(&self, path: &str, cookie: &str) -> Response {
        self.client
            .get(format!("{}{path}", self.base))
            .header("cookie", cookie)
            .send()
            .await
            .unwrap()
    }
    async fn json(&self, path: &str, cookie: &str, status: StatusCode) -> Value {
        let response = self.get(path, cookie).await;
        assert_eq!(response.status(), status, "GET {path}");
        assert_eq!(response.headers()["cache-control"], "no-store");
        response.json().await.unwrap()
    }
    async fn stop(mut self) {
        self.child.kill().await.unwrap();
        self.child.wait().await.unwrap();
    }
}

fn cookie(response: &Response, name: &str) -> String {
    response
        .headers()
        .get_all("set-cookie")
        .iter()
        .find_map(|h| {
            let part = h.to_str().unwrap().split(';').next().unwrap();
            part.starts_with(&format!("{name}="))
                .then(|| part.to_owned())
        })
        .unwrap_or_else(|| panic!("missing {name} cookie"))
}

async fn session(pool: &PgPool, user: Uuid) -> String {
    let token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
    let csrf = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
    sqlx::query("INSERT INTO browser_sessions(token_hash,user_id,csrf_hash,expires_at,verified_email) SELECT $1,id,$3,now()+interval '1 hour',email FROM users WHERE id=$2")
        .bind(Sha256::digest(token.as_bytes()).to_vec()).bind(user)
        .bind(Sha256::digest(csrf.as_bytes()).to_vec()).execute(pool).await.unwrap();
    format!("omg_session={token}; omg_csrf={csrf}")
}

async fn execution(pool: &PgPool, workspace: Uuid, key: Uuid, id: Uuid, at: &str) {
    // Disabled bootstrap deployment: these rows exercise reporting, never dispatch.
    sqlx::query("INSERT INTO inference_executions(id,workspace_id,api_key_id,deployment_id,public_model,provider,streamed,state,root_request_id,started_at,completed_at) SELECT $1,$2,$3,id,'company/smart','openai',false,'succeeded',$1,$4::text::timestamptz,$4::text::timestamptz FROM deployments LIMIT 1")
        .bind(id).bind(workspace).bind(key).bind(at).execute(pool).await.unwrap();
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn running_gateway_wire_dates_and_negative_epoch_pagination(pool: PgPool) {
    let seed = bootstrap::seed(&Store::new(pool.clone()), Environment::Development)
        .await
        .unwrap()
        .unwrap();
    let user: Uuid = sqlx::query_scalar("SELECT issued_to_user_id FROM api_keys WHERE id=$1")
        .bind(seed.team_key.id)
        .fetch_one(&pool)
        .await
        .unwrap();
    let browser = session(&pool, user).await;
    let issuer = Issuer::start().await;
    let gateway = Gateway::start(&pool, &issuer).await;
    let ws = seed.team_workspace_id;

    // key_stats decodes timestamptz and serializes it in Rust. This is distinct
    // from generation rows, whose timestamps are rendered by PostgreSQL JSON.
    for (at, expected) in [
        ("1969-12-31T23:59:59Z", "1969-12-31T23:59:59Z"),
        ("2000-01-01T00:00:00.123Z", "2000-01-01T00:00:00.123Z"),
        ("2000-01-01T00:00:00.1234Z", "2000-01-01T00:00:00.123400Z"),
        ("2000-01-01T00:00:00.123456Z", "2000-01-01T00:00:00.123456Z"),
    ] {
        let id = Uuid::new_v4();
        execution(&pool, ws, seed.team_key.id, id, at).await;
        let stats = gateway
            .json(
                &format!("/api/v1/workspaces/{ws}/keys/{}/stats", seed.team_key.id),
                &browser,
                StatusCode::OK,
            )
            .await;
        assert_eq!(stats["last_used_at"], expected);
        sqlx::query("DELETE FROM inference_executions WHERE id=$1")
            .bind(id)
            .execute(&pool)
            .await
            .unwrap();
    }

    // A one-microsecond step before Unix epoch, two equal instants, and exclusive
    // midnight boundaries make truncation/sign and tie-order mistakes observable.
    for (id, at) in [
        (1, "1969-12-30T00:00:00Z"),
        (2, "1969-12-30T23:59:59.123456Z"),
        (3, "1969-12-30T23:59:59.123456Z"),
        (4, "1969-12-30T23:59:59.123457Z"),
        (5, "1969-12-31T00:00:00Z"),
        (6, "1969-12-29T23:59:59.999999Z"),
    ] {
        execution(&pool, ws, seed.team_key.id, Uuid::from_u128(id), at).await;
    }
    for (resource, field) in [
        ("requests", "root_request_id"),
        ("generations", "execution_id"),
    ] {
        let path = format!(
            "/api/v1/workspaces/{ws}/{resource}?start_date=1969-12-30&end_date=1969-12-31&limit=1"
        );
        let mut next = String::new();
        for (id, micros, expected_time) in [
            (
                4,
                Some(-86400876543_i64),
                "1969-12-30T23:59:59.123457+00:00",
            ),
            (
                3,
                Some(-86400876544_i64),
                "1969-12-30T23:59:59.123456+00:00",
            ),
            (
                2,
                Some(-86400876544_i64),
                "1969-12-30T23:59:59.123456+00:00",
            ),
            (1, None, "1969-12-30T00:00:00+00:00"),
        ] {
            let page = gateway
                .json(&format!("{path}{next}"), &browser, StatusCode::OK)
                .await;
            assert_eq!(page["data"].as_array().unwrap().len(), 1);
            assert_eq!(page["data"][0][field], Uuid::from_u128(id).to_string());
            assert_eq!(page["data"][0]["started_at"], expected_time);
            if let Some(micros) = micros {
                let expected = format!("{micros}_{}", Uuid::from_u128(id));
                assert_eq!(page["next_cursor"], expected);
                next = format!("&cursor={expected}");
            } else {
                assert!(page["next_cursor"].is_null());
            }
        }
        for (micros, expected_rows) in [("-210866803200000000", 0), ("8210266876799999999", 1)] {
            let page = gateway
                .json(
                    &format!("{path}&cursor={micros}_{}", Uuid::nil()),
                    &browser,
                    StatusCode::OK,
                )
                .await;
            assert_eq!(page["data"].as_array().unwrap().len(), expected_rows);
        }
        for bad in [
            "-210866803200000001",
            "8210266876800000000",
            "9223372036854775807",
            "-9223372036854775808",
            "9223372036854775808",
        ] {
            gateway
                .json(
                    &format!("{path}&cursor={bad}_{}", Uuid::nil()),
                    &browser,
                    StatusCode::BAD_REQUEST,
                )
                .await;
        }
    }
    let report = gateway
        .json(
            &format!(
                "/api/v1/workspaces/{ws}/cost-report?start_date=1969-12-30&end_date=1969-12-31"
            ),
            &browser,
            StatusCode::OK,
        )
        .await;
    assert_eq!(report["totals"]["attempts"], "4");
    assert_eq!(report["daily"][0]["date"], "1969-12-30");
    assert_eq!(report["daily"][0]["totals"]["attempts"], "4");
    gateway
        .json(
            &format!(
                "/api/v1/workspaces/{ws}/generations?start_date=1969-12-30&end_date=1969-12-31"
            ),
            "",
            StatusCode::UNAUTHORIZED,
        )
        .await;
    report_frontiers(&gateway, &browser, ws).await;
    mapping_timestamp_frontiers(&pool, &gateway, &browser).await;
    gateway.stop().await;
}

async fn report_frontiers(gateway: &Gateway, browser: &str, ws: Uuid) {
    let report = format!("/api/v1/workspaces/{ws}/cost-report");
    let leap = gateway
        .json(
            &format!("{report}?start_date=2024-02-28&end_date=2024-03-01&compare=previous_period"),
            browser,
            StatusCode::OK,
        )
        .await;
    assert_eq!(leap["period"]["start_date"], "2024-02-28");
    assert_eq!(leap["period"]["end_date"], "2024-03-01");
    assert_eq!(
        leap["daily"]
            .as_array()
            .unwrap()
            .iter()
            .map(|d| d["date"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["2024-02-28", "2024-02-29"]
    );
    assert_eq!(leap["comparison"]["period"]["start_date"], "2024-02-26");
    assert_eq!(leap["comparison"]["period"]["end_date"], "2024-02-28");
    let max = gateway
        .json(
            &format!("{report}?start_date=2024-01-01&end_date=2024-04-03"),
            browser,
            StatusCode::OK,
        )
        .await;
    assert_eq!(max["daily"].as_array().unwrap().len(), 93);
    // A cursor or malformed date must never silently expand the caller's period.
    for query in [
        "start_date=2024-01-01&end_date=2024-04-04",
        "start_date=2023-02-29&end_date=2023-03-01",
        "start_date=1900-02-29&end_date=1900-03-01",
        "start_date=2024-02-29&end_date=2024-02-29",
        "start_date=2024-03-01&end_date=2024-02-29",
        "start_date=2024-02-29T00:00:00Z&end_date=2024-03-01",
        "start_date=2024-02-29%2B00:00&end_date=2024-03-01",
        "start_date=2024-02-29&end_date=2024-03-01&start_date=2024-01-01",
        "start_date=0000-01-01&end_date=0000-01-02",
        "start_date=9999-12-30&end_date=9999-12-31",
    ] {
        gateway
            .json(
                &format!("{report}?{query}"),
                browser,
                StatusCode::BAD_REQUEST,
            )
            .await;
    }
    let century = gateway
        .json(
            &format!("{report}?start_date=2000-02-29&end_date=2000-03-01"),
            browser,
            StatusCode::OK,
        )
        .await;
    assert_eq!(century["daily"][0]["date"], "2000-02-29");
}

async fn mapping_timestamp_frontiers(pool: &PgPool, gateway: &Gateway, browser: &str) {
    let connection = Uuid::new_v4();
    sqlx::query("INSERT INTO provider_connections(id,name,provider,credential_ref) VALUES($1,'Date fixture','anthropic','env:UNRESOLVED_FIXTURE')")
        .bind(connection).execute(pool).await.unwrap();
    let csrf = browser.split("omg_csrf=").nth(1).unwrap();
    let path = "/api/v1/platform/provider-cost-mappings";
    // This endpoint uses typed SQL timestamp binding rather than JSON-to-recordset.
    // Submicrosecond differences truncate toward PostgreSQL's 2000 epoch. The
    // pre-1970 and 1999 cases distinguish that from flooring or truncating Unix time.
    for (index, input, expected) in [
        (
            0,
            "1969-12-31T23:59:59.123456789Z",
            "1969-12-31T23:59:59.123457+00:00",
        ),
        (
            1,
            "2000-01-01T00:00:00.123456789Z",
            "2000-01-01T00:00:00.123456+00:00",
        ),
        (
            2,
            "2000-01-01T01:00:00.123400000+01:00",
            "2000-01-01T00:00:00.1234+00:00",
        ),
        (3, "2016-12-31T23:59:60.5Z", "2017-01-01T00:00:00.5+00:00"),
        (
            4,
            "1999-12-31T23:59:59.123456789Z",
            "1999-12-31T23:59:59.123457+00:00",
        ),
    ] {
        let body = json!({"account_id":format!("date-{index}"),"evidence_reference":"fixture:date","supersedes":null,
            "bindings":[{"provider_workspace_id":null,"connection_id":connection,"deployment_id":null,"workspace_id":null,
            "effective_start":input,"effective_end":"2017-01-02T00:00:00Z","exclusive":false,"non_omg_traffic":false,
            "attested":true,"evidence_reference":"fixture:date","model_mappings":[]}]});
        let response = gateway
            .client
            .post(format!("{}{path}", gateway.base))
            .header("cookie", browser)
            .header("origin", &gateway.base)
            .header("x-csrf-token", csrf)
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "mapping {index}");
        let id = response.json::<Value>().await.unwrap()["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let saved = gateway
            .json(&format!("{path}/{id}"), browser, StatusCode::OK)
            .await;
        assert_eq!(saved["bindings"][0]["effective_start"], expected);
    }
    for input in [
        "2023-02-29T00:00:00Z",
        "2000-01-01T24:00:00Z",
        "2000-01-01T00:00:00+24:00",
        "2000-01-01T00:00:00",
    ] {
        let body = json!({"account_id":"invalid-date","evidence_reference":"fixture:date","supersedes":null,
            "bindings":[{"provider_workspace_id":null,"connection_id":connection,"deployment_id":null,"workspace_id":null,
            "effective_start":input,"effective_end":"2017-01-02T00:00:00Z","exclusive":false,"non_omg_traffic":false,
            "attested":true,"evidence_reference":"fixture:date","model_mappings":[]}]});
        let response = gateway
            .client
            .post(format!("{}{path}", gateway.base))
            .header("cookie", browser)
            .header("origin", &gateway.base)
            .header("x-csrf-token", csrf)
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid mapping timestamp {input}"
        );
    }
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM provider_cost_mappings WHERE account_id='invalid-date'"
        )
        .fetch_one(pool)
        .await
        .unwrap(),
        0
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn running_gateway_signed_oidc_numeric_dates_and_session_expiry(pool: PgPool) {
    let issuer = Issuer::start().await;
    sqlx::query("INSERT INTO oidc_group_mappings(id,issuer,group_value,target_kind,platform_role) VALUES($1,$2,'entitled','platform','user')")
        .bind(Uuid::new_v4()).bind(&issuer.base).execute(&pool).await.unwrap();
    let gateway = Gateway::start(&pool, &issuer).await;
    // Use wide deterministic margins around the wall clock; no sleeps decide
    // token validity. The issuer signs even invalid raw NumericDate values.
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    for (label, field, value, status) in [
        ("expired", "exp", json!(now - 60), StatusCode::BAD_REQUEST),
        ("pre-epoch", "exp", json!(-0.5), StatusCode::BAD_REQUEST),
        (
            "beyond carrier",
            "exp",
            json!(i64::MAX),
            StatusCode::BAD_REQUEST,
        ),
        (
            "string expiry",
            "exp",
            json!("2099-01-01T00:00:00Z"),
            StatusCode::BAD_REQUEST,
        ),
        (
            "wrong issuer",
            "iss",
            json!("https://wrong.invalid"),
            StatusCode::BAD_REQUEST,
        ),
        (
            "wrong audience",
            "aud",
            json!("other"),
            StatusCode::BAD_REQUEST,
        ),
        (
            "wrong nonce",
            "nonce",
            json!("wrong"),
            StatusCode::BAD_REQUEST,
        ),
        ("wrong azp", "azp", json!("other"), StatusCode::BAD_REQUEST),
        (
            "multiple audiences without azp",
            "aud",
            json!(["date-contract", "date-contract"]),
            StatusCode::BAD_REQUEST,
        ),
        (
            "unverified email",
            "email_verified",
            json!(false),
            StatusCode::BAD_REQUEST,
        ),
        ("no entitlement", "groups", json!([]), StatusCode::FORBIDDEN),
        (
            "invalid groups",
            "groups",
            json!([123]),
            StatusCode::FORBIDDEN,
        ),
    ] {
        let (state, browser, mut claims) = begin_login(&gateway, &issuer, now).await;
        claims[field] = value;
        *issuer.claims.lock().unwrap() = claims;
        let response = gateway
            .get(
                &format!("/api/v1/auth/callback?state={state}&code=fixture"),
                &browser,
            )
            .await;
        assert_eq!(response.status(), status, "{label}");
        assert!(
            response
                .headers()
                .get_all("set-cookie")
                .iter()
                .all(|h| !h.to_str().unwrap().starts_with("omg_session=")),
            "{label} issued a session"
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM browser_sessions")
                .fetch_one(&pool)
                .await
                .unwrap(),
            0,
            "{label} persisted a session"
        );
    }
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM workspaces WHERE kind='personal'")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
    let (state, browser, claims) = begin_login(&gateway, &issuer, now).await;
    *issuer.claims.lock().unwrap() = claims;
    sqlx::query("UPDATE oidc_login_attempts SET expires_at=clock_timestamp()-interval '1 microsecond' WHERE state_hash=$1")
        .bind(Sha256::digest(state.as_bytes()).to_vec()).execute(&pool).await.unwrap();
    let ended = gateway
        .get(
            &format!("/api/v1/auth/callback?state={state}&code=fixture"),
            &browser,
        )
        .await;
    assert_eq!(ended.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM browser_sessions")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
    // Matching azp does not authorize an additional, untrusted audience.
    let (state, browser, mut claims) = begin_login(&gateway, &issuer, now).await;
    claims["aud"] = json!(["date-contract", "other"]);
    claims["azp"] = json!("date-contract");
    *issuer.claims.lock().unwrap() = claims;
    let untrusted = gateway
        .get(
            &format!("/api/v1/auth/callback?state={state}&code=fixture"),
            &browser,
        )
        .await;
    assert_eq!(untrusted.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM browser_sessions")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
    // Signed fractional NumericDate and pre-epoch iat both remain accepted.
    let (state, browser, mut claims) = begin_login(&gateway, &issuer, now).await;
    claims["exp"] = json!(now as f64 + 300.125);
    claims["iat"] = json!(-0.5);
    claims["aud"] = json!("date-contract");
    claims["azp"] = json!("date-contract");
    *issuer.claims.lock().unwrap() = claims;
    let response = gateway
        .get(
            &format!("/api/v1/auth/callback?state={state}&code=fixture"),
            &browser,
        )
        .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let browser = cookie(&response, "omg_session");
    let me = gateway.json("/api/v1/me", &browser, StatusCode::OK).await;
    assert_eq!(me["user"]["email"], "date@example.test");
    assert_eq!(me["user"]["platform_role"], "user");
    assert_eq!(me["workspaces"].as_array().unwrap().len(), 1);
    // Expiring the absolute deadline in PostgreSQL blocks the *next* HTTP request,
    // regardless of the application's clock or the still-present browser cookie.
    sqlx::query(
        "UPDATE browser_sessions SET expires_at=clock_timestamp()-interval '1 microsecond'",
    )
    .execute(&pool)
    .await
    .unwrap();
    gateway
        .json("/api/v1/me", &browser, StatusCode::UNAUTHORIZED)
        .await;
    let (state, browser, claims) = begin_login(&gateway, &issuer, now).await;
    *issuer.claims.lock().unwrap() = claims;
    let response = gateway
        .get(
            &format!("/api/v1/auth/callback?state={state}&code=fixture"),
            &browser,
        )
        .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let browser = cookie(&response, "omg_session");
    gateway.json("/api/v1/me", &browser, StatusCode::OK).await;
    sqlx::query(
        "UPDATE browser_sessions SET idle_expires_at=clock_timestamp()-interval '1 microsecond'",
    )
    .execute(&pool)
    .await
    .unwrap();
    gateway
        .json("/api/v1/me", &browser, StatusCode::UNAUTHORIZED)
        .await;
    gateway.stop().await;
}

async fn begin_login(gateway: &Gateway, issuer: &Issuer, now: u64) -> (String, String, Value) {
    let response = gateway.get("/api/v1/auth/login", "").await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let url = reqwest::Url::parse(response.headers()["location"].to_str().unwrap()).unwrap();
    let params: std::collections::HashMap<_, _> = url
        .query_pairs()
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    let claims = json!({"iss":issuer.base,"sub":"date-subject","aud":"date-contract","exp":now+300,
        "iat":now,"nonce":params["nonce"],"email":"Date@Example.test","email_verified":true,"groups":["entitled"]});
    (
        params["state"].clone(),
        cookie(&response, "omg_oidc"),
        claims,
    )
}
