//! Real serve process + loopback receiver, no provider or production access.
#![cfg(feature = "integration-tests")]
#![allow(clippy::disallowed_methods)]
use axum::{
    Router,
    body::Bytes,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::post,
};
use open_model_gateway::{admin_tokens, governance::locks};
use serde_json::{Value, json};
use sqlx::{ConnectOptions, PgPool};
use std::{
    collections::HashMap,
    process::Stdio,
    sync::{Arc, Mutex},
    time::Duration,
};
use uuid::Uuid;

const KEY: &str = "01234567890123456789012345678901";
const OLD: &str = "abcdefghijklmnopqrstuvwxyz012345";
#[derive(Default)]
struct Receiver {
    seen: HashMap<String, usize>,
    errors: Vec<String>,
    bodies: Vec<Value>,
}
async fn receive(
    State(state): State<Arc<Mutex<Receiver>>>,
    Path(path): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    // Deliberately independent implementation: Python's stdlib HMAC and
    // compare_digest, timestamp/replay contract and header/body-id equality.
    let mut verifier=std::process::Command::new("python3").arg("-c").arg("import sys,json,hmac,hashlib,time\nx=json.load(sys.stdin); b=x['body'].encode(); p=x['signature'].split(','); t=int(p[0][2:]); assert abs(time.time()-t)<=300; assert len(p)==len(x['keys'])+1\nfor key in x['keys']:\n expected=hmac.new(key.encode(),str(t).encode()+b'.'+b,hashlib.sha256).hexdigest(); assert any(hmac.compare_digest(s,'v1='+expected) for s in p[1:])\nassert json.loads(b)['delivery_id']==x['delivery']\n").stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
    use std::io::Write;
    let envelope: Value = serde_json::from_slice(&body).unwrap();
    let keys = if path == "retry" && envelope["test"] == false {
        vec![KEY, OLD]
    } else {
        vec![KEY]
    };
    let data = json!({"body":std::str::from_utf8(&body).unwrap(),"signature":headers["x-omg-signature"].to_str().unwrap(),"delivery":headers["x-omg-delivery"].to_str().unwrap(),"keys":keys});
    verifier
        .stdin
        .take()
        .unwrap()
        .write_all(data.to_string().as_bytes())
        .unwrap();
    let verified = verifier.wait().unwrap().success();
    let count = {
        let mut state = state.lock().unwrap();
        if !verified {
            state.errors.push("signature verification failed".into());
        }
        state.bodies.push(serde_json::from_slice(&body).unwrap());
        let count = state.seen.entry(path.clone()).or_default();
        *count += 1;
        *count
    };
    match path.as_str() {
        "retry" if count == 1 => (StatusCode::SERVICE_UNAVAILABLE, HeaderMap::new(), vec![]),
        "redirect" => {
            let mut h = HeaderMap::new();
            h.insert("location", "/trap".parse().unwrap());
            (StatusCode::TEMPORARY_REDIRECT, h, vec![])
        }
        "large" => (
            StatusCode::OK,
            HeaderMap::new(),
            vec![b'x'; 2 * 1024 * 1024],
        ),
        _ => (StatusCode::NO_CONTENT, HeaderMap::new(), vec![]),
    }
}
async fn wait_state(pool: &PgPool, id: Uuid, expected: &str) {
    tokio::time::timeout(Duration::from_secs(25), async {
        loop {
            let state: String =
                sqlx::query_scalar("SELECT state FROM alert_webhook_outbox WHERE id=$1")
                    .bind(id)
                    .fetch_one(pool)
                    .await
                    .unwrap();
            if state == expected {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("delivery {id} never became {expected}"));
}
fn start(database: &str, address: &str, allowlist: &[String]) -> tokio::process::Child {
    tokio::process::Command::new(env!("CARGO_BIN_EXE_open-model-gateway"))
        .arg("serve")
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap())
        .env("DATABASE_URL", database)
        .env("GATEWAY_ENV_FILE", "/dev/null")
        .env("GATEWAY_ENV", "development")
        .env("GATEWAY_LISTEN", address)
        .env("GATEWAY_ALERT_INTERVAL_SECONDS", "1")
        .env("GATEWAY_ALERT_WEBHOOK_URL_ALLOWLIST", allowlist.join(","))
        .env(
            "GATEWAY_ALERT_WEBHOOK_SECRET_ENV_ALLOWLIST",
            "HOOK_ACCEPTANCE,HOOK_PREVIOUS",
        )
        .env("HOOK_ACCEPTANCE", KEY)
        .env("HOOK_PREVIOUS", OLD)
        .env("HTTP_PROXY", "http://127.0.0.1:9")
        .env("HTTPS_PROXY", "http://127.0.0.1:9")
        .env("ALL_PROXY", "http://127.0.0.1:9")
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .unwrap()
}
async fn ready(child: &mut tokio::process::Child, client: &reqwest::Client, base: &str) {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            assert!(
                child.try_wait().unwrap().is_none(),
                "gateway exited before readiness"
            );
            if client
                .get(format!("{base}/health/ready"))
                .send()
                .await
                .is_ok_and(|r| r.status().is_success())
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("gateway readiness");
}
async fn create(client: &reqwest::Client, base: &str, token: &str, url: &str) -> Uuid {
    let response = client
        .post(format!("{base}/api/v1/platform/alerts/webhooks"))
        .bearer_auth(token)
        .json(&json!({"label":"acceptance","url":url,"current_secret_ref":"env:HOOK_ACCEPTANCE"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let data: Value = response.json().await.unwrap();
    let id: Uuid = data["id"].as_str().unwrap().parse().unwrap();
    assert_eq!(
        client
            .patch(format!("{base}/api/v1/platform/alerts/webhooks/{id}"))
            .bearer_auth(token)
            .json(&json!({"enabled":true}))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    id
}
async fn test(client: &reqwest::Client, base: &str, token: &str, id: Uuid) -> Uuid {
    let response = client
        .post(format!("{base}/api/v1/platform/alerts/webhooks/{id}/test"))
        .bearer_auth(token)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    response.json::<Value>().await.unwrap()["id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap()
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn serve_signed_fire_clear_restart_fixed_test_and_refusals(pool: PgPool) {
    let owner = Uuid::new_v4();
    sqlx::query("INSERT INTO users(id,email) VALUES($1,'webhook-acceptance@example.test')")
        .bind(owner)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO platform_role_grants(user_id,role,source) VALUES($1,'admin','manual')",
    )
    .bind(owner)
    .execute(&pool)
    .await
    .unwrap();
    let mut tx = pool.begin().await.unwrap();
    locks::exclusive(&mut tx, [locks::Scope::User(owner)])
        .await
        .unwrap();
    let token = admin_tokens::create_admin_token(
        &mut tx,
        owner,
        "acceptance",
        &["settings:write".into(), "platform:read".into()],
        30,
        None,
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let receiver = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let receiver_base = format!("http://{}", receiver.local_addr().unwrap());
    let seen = Arc::new(Mutex::new(Receiver::default()));
    let receiver_task = tokio::spawn(
        axum::serve(
            receiver,
            Router::new()
                .route("/{path}", post(receive))
                .with_state(seen.clone()),
        )
        .into_future(),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    drop(listener);
    let base = format!("http://{addr}");
    let urls = vec![
        format!("{receiver_base}/retry"),
        format!("{receiver_base}/redirect"),
        format!("{receiver_base}/large"),
        "https://10.0.0.1/hook".into(),
        "https://169.254.169.254/hook".into(),
    ];
    let database = pool.connect_options().to_url_lossy().to_string();
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let mut gateway = start(&database, &addr, &urls);
    ready(&mut gateway, &client, &base).await;
    let dest = create(&client, &base, &token.token, &urls[0]).await;
    assert_eq!(client.patch(format!("{base}/api/v1/platform/alerts/webhooks/{dest}")).bearer_auth(&token.token).json(&json!({"previous_secret_ref":"env:HOOK_PREVIOUS","previous_secret_until":(chrono::Utc::now()+chrono::Duration::hours(1)).to_rfc3339()})).send().await.unwrap().status(),200);
    let event:Uuid=sqlx::query_scalar("INSERT INTO alert_events(id,builtin,kind,subject_key,level,severity,summary) VALUES(gen_random_uuid(),'history_orphans','history_orphans',gen_random_uuid()::text,1,'warning','PRIVATE CANARY') RETURNING id").fetch_one(&pool).await.unwrap();
    sqlx::query(
        "INSERT INTO alert_deliveries(id,event_id,transition) VALUES(gen_random_uuid(),$1,'fired')",
    )
    .bind(event)
    .execute(&pool)
    .await
    .unwrap();
    let fired: Uuid = sqlx::query_scalar(
        "SELECT id FROM alert_webhook_outbox WHERE destination_id=$1 AND event_id=$2",
    )
    .bind(dest)
    .bind(event)
    .fetch_one(&pool)
    .await
    .unwrap();
    // First 503 schedules +60 seconds. Wait for that durable outcome, then
    // restart the actual binary and advance only the disposable test's due time.
    tokio::time::timeout(Duration::from_secs(25), async {
        loop {
            if sqlx::query_scalar::<_, i32>(
                "SELECT attempt_count FROM alert_webhook_outbox WHERE id=$1 AND state='waiting'",
            )
            .bind(fired)
            .fetch_optional(&pool)
            .await
            .unwrap()
                == Some(1)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .unwrap();
    gateway.kill().await.unwrap();
    gateway.wait().await.unwrap();
    sqlx::query("UPDATE alert_webhook_outbox SET next_attempt_at=clock_timestamp() WHERE id=$1")
        .bind(fired)
        .execute(&pool)
        .await
        .unwrap();
    gateway = start(&database, &addr, &urls);
    ready(&mut gateway, &client, &base).await;
    wait_state(&pool, fired, "sent").await;
    sqlx::query(
        "UPDATE alert_events SET resolved_at=clock_timestamp(),resolution='cleared' WHERE id=$1",
    )
    .bind(event)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO alert_deliveries(id,event_id,transition) VALUES(gen_random_uuid(),$1,'resolved')").bind(event).execute(&pool).await.unwrap();
    let cleared:Uuid=sqlx::query_scalar("SELECT id FROM alert_webhook_outbox WHERE destination_id=$1 AND event_id=$2 AND transition='resolved'").bind(dest).bind(event).fetch_one(&pool).await.unwrap();
    wait_state(&pool, cleared, "sent").await;
    assert_eq!(
        client
            .patch(format!("{base}/api/v1/platform/alerts/webhooks/{dest}"))
            .bearer_auth(&token.token)
            .json(&json!({"previous_secret_ref":null,"previous_secret_until":null}))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    let fixed = test(&client, &base, &token.token, dest).await;
    wait_state(&pool, fixed, "sent").await;
    // Refusal fixtures queue through the same constructor as test-send, with
    // no bypass of destination or payload guards (only the API rate limiter).
    for (url, expected, error) in [
        (&urls[1], "failed", Some("rejected")),
        (&urls[2], "sent", None),
        (&urls[3], "failed", Some("host_not_allowed")),
        (&urls[4], "failed", Some("host_not_allowed")),
    ] {
        let d = create(&client, &base, &token.token, url).await;
        let id = Uuid::new_v4();
        sqlx::query("INSERT INTO alert_webhook_outbox(id,destination_id,destination_generation,test,payload) SELECT $1,$2,enable_generation,true,convert_to(alert_webhook_payload($1,NULL,NULL),'UTF8') FROM alert_webhook_destinations WHERE id=$2").bind(id).bind(d).execute(&pool).await.unwrap();
        wait_state(&pool, id, expected).await;
        assert_eq!(
            sqlx::query_scalar::<_, Option<String>>(
                "SELECT last_error FROM alert_webhook_outbox WHERE id=$1"
            )
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap()
            .as_deref(),
            error
        );
    }
    let response = client
        .get(format!(
            "{base}/api/v1/platform/alerts/webhooks/{dest}/deliveries"
        ))
        .bearer_auth(&token.token)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let status: Value = response.json().await.unwrap();
    assert_eq!(status["counts"]["sent"], 3);
    {
        let seen = seen.lock().unwrap();
        assert!(seen.errors.is_empty(), "{:?}", seen.errors);
        assert!(!seen.seen.contains_key("trap"));
        assert_eq!(seen.seen["retry"], 4);
        let fixed = seen
            .bodies
            .iter()
            .find(|b| b["delivery_id"] == fixed.to_string())
            .unwrap();
        assert_eq!(fixed["test"], true);
        assert!(fixed["event"].is_null());
        assert!(
            !serde_json::to_string(&seen.bodies)
                .unwrap()
                .contains("PRIVATE CANARY")
        );
    }
    gateway.kill().await.unwrap();
    gateway.wait().await.unwrap();
    receiver_task.abort();
}
