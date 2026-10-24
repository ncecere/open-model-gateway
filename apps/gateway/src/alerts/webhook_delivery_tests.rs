use super::*;
use sqlx::PgPool;

async fn destination(pool: &PgPool) -> Uuid {
    sqlx::query_scalar("INSERT INTO alert_webhook_destinations(label,url,current_secret_ref,enabled) VALUES('receiver','https://example.test/hook','env:HOOK',true) RETURNING id").fetch_one(pool).await.unwrap()
}
async fn queue(pool: &PgPool, dest: Uuid, test: bool) -> Uuid {
    if test {
        return sqlx::query_scalar("INSERT INTO alert_webhook_outbox(id,destination_id,destination_generation,test,payload) SELECT i,$1,(SELECT enable_generation FROM alert_webhook_destinations WHERE id=$1),true,convert_to(alert_webhook_payload(i,NULL,NULL),'UTF8') FROM (SELECT gen_random_uuid() i) x RETURNING id").bind(dest).fetch_one(pool).await.unwrap();
    }
    let event: Uuid = sqlx::query_scalar("INSERT INTO alert_events(id,builtin,kind,subject_key,level,severity,summary) VALUES(gen_random_uuid(),'scim_last_admin','scim_last_admin',gen_random_uuid()::text,1,'critical','private canary') RETURNING id").fetch_one(pool).await.unwrap();
    sqlx::query_scalar("INSERT INTO alert_webhook_outbox(id,destination_id,destination_generation,event_id,transition,payload) SELECT i,$1,(SELECT enable_generation FROM alert_webhook_destinations WHERE id=$1),$2,'fired',convert_to(alert_webhook_payload(i,$2,'fired'),'UTF8') FROM (SELECT gen_random_uuid() i) x RETURNING id").bind(dest).bind(event).fetch_one(pool).await.unwrap()
}
async fn claimed(s: &Store) -> Claim {
    claim(s, Uuid::new_v4()).await.unwrap().unwrap().unwrap()
}
async fn state(pool: &PgPool, id: Uuid) -> (String, i32, Option<String>) {
    sqlx::query_as("SELECT state,attempt_count,last_error FROM alert_webhook_outbox WHERE id=$1")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn concurrent_claimers_and_stale_completion(pool: PgPool) {
    let d = destination(&pool).await;
    let id = queue(&pool, d, false).await;
    let s = Store::new(pool.clone());
    let (a, b) = tokio::join!(claim(&s, Uuid::new_v4()), claim(&s, Uuid::new_v4()));
    let c = match (a.unwrap(), b.unwrap()) {
        (Some(Some(c)), None) | (None, Some(Some(c))) => c,
        _ => panic!("one row can have only one lease"),
    };
    assert_eq!(state(&pool, id).await.1, 1);
    sqlx::query("UPDATE alert_webhook_outbox SET lease_expires_at=clock_timestamp()-interval '1 second' WHERE id=$1").bind(id).execute(&pool).await.unwrap();
    let successor = claimed(&s).await;
    assert!(!complete(&s, &c, Ok(())).await.unwrap());
    assert!(complete(&s, &successor, Ok(())).await.unwrap());
    assert_eq!(state(&pool, id).await, ("sent".into(), 2, None));
    assert!(
        !complete(&s, &successor, Err(Failure::Connection))
            .await
            .unwrap()
    );
    assert!(
        sqlx::query(
            "UPDATE alert_webhook_outbox SET state='waiting',completed_at=NULL WHERE id=$1"
        )
        .bind(id)
        .execute(&pool)
        .await
        .is_err()
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn retry_backoffs_single_tests_and_terminal_audits(pool: PgPool) {
    let d = destination(&pool).await;
    let s = Store::new(pool.clone());
    let id = queue(&pool, d, false).await;
    for attempt in 1..=5 {
        let c = claimed(&s).await;
        assert_eq!(c.row.id, id);
        assert!(
            complete(&s, &c, Err(Failure::TransientStatus))
                .await
                .unwrap()
        );
        let (status, n, error) = state(&pool, id).await;
        assert_eq!(n, attempt);
        assert_eq!(error.as_deref(), Some("rejected"));
        if attempt < 5 {
            assert_eq!(status, "waiting");
            let seconds: f64 = sqlx::query_scalar("SELECT extract(epoch FROM next_attempt_at-clock_timestamp())::float8 FROM alert_webhook_outbox WHERE id=$1").bind(id).fetch_one(&pool).await.unwrap();
            assert!((seconds - f64::from(BACKOFF[attempt as usize - 1])).abs() < 3.0);
            assert!(claim(&s, Uuid::new_v4()).await.unwrap().is_none());
            sqlx::query(
                "UPDATE alert_webhook_outbox SET next_attempt_at=clock_timestamp() WHERE id=$1",
            )
            .bind(id)
            .execute(&pool)
            .await
            .unwrap();
        } else {
            assert_eq!(status, "exhausted");
        }
    }
    let id = queue(&pool, d, true).await;
    let c = claimed(&s).await;
    complete(&s, &c, Err(Failure::Timeout)).await.unwrap();
    assert_eq!(
        state(&pool, id).await,
        ("failed".into(), 1, Some("timeout".into()))
    );
    let audits: Vec<String> = sqlx::query_scalar(
        "SELECT action FROM audit_events WHERE action LIKE 'alert_webhook.%' ORDER BY created_at",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(audits, ["alert_webhook.exhausted", "alert_webhook.failed"]);
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn late_generation_disabled_deleted_and_scrubbed_refused(pool: PgPool) {
    let d = destination(&pool).await;
    let s = Store::new(pool.clone());
    // Enqueue owns only KEY SHARE, so management can disable and re-enable
    // while its old-generation row remains invisible to management's trigger.
    let mut enqueue = pool.begin().await.unwrap();
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO alert_webhook_outbox(id,destination_id,destination_generation,test,payload) SELECT $1,$2,enable_generation,true,convert_to(alert_webhook_payload($1,NULL,NULL),'UTF8') FROM alert_webhook_destinations WHERE id=$2").bind(id).bind(d).execute(&mut *enqueue).await.unwrap();
    sqlx::query("UPDATE alert_webhook_destinations SET enabled=false WHERE id=$1")
        .bind(d)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE alert_webhook_destinations SET enabled=true WHERE id=$1")
        .bind(d)
        .execute(&pool)
        .await
        .unwrap();
    enqueue.commit().await.unwrap();
    assert_eq!(state(&pool, id).await.0, "waiting");
    assert!(matches!(
        claim(&s, Uuid::new_v4()).await.unwrap(),
        Some(None)
    ));
    assert_eq!(state(&pool, id).await.0, "ineligible");
    for delete in [false, true] {
        let d = destination(&pool).await;
        let mut enqueue = pool.begin().await.unwrap();
        let id = Uuid::new_v4();
        sqlx::query("INSERT INTO alert_webhook_outbox(id,destination_id,destination_generation,test,payload) VALUES($1,$2,0,true,convert_to(alert_webhook_payload($1,NULL,NULL),'UTF8'))").bind(id).bind(d).execute(&mut *enqueue).await.unwrap();
        sqlx::query(if delete {"UPDATE alert_webhook_destinations SET enabled=false,deleted_at=clock_timestamp() WHERE id=$1"} else {"UPDATE alert_webhook_destinations SET enabled=false WHERE id=$1"}).bind(d).execute(&pool).await.unwrap();
        enqueue.commit().await.unwrap();
        assert!(matches!(
            claim(&s, Uuid::new_v4()).await.unwrap(),
            Some(None)
        ));
        assert_eq!(state(&pool, id).await.0, "ineligible");
    }
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn claim_share_serializes_management_without_blocking_enqueue(pool: PgPool) {
    let d = destination(&pool).await;
    let id = queue(&pool, d, false).await;
    let s = Store::new(pool.clone());
    let mut management = pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM alert_webhook_destinations WHERE id=$1 FOR NO KEY UPDATE")
        .bind(d)
        .execute(&mut *management)
        .await
        .unwrap();
    // Claim skips the management-locked destination, and owns no queue lock.
    assert!(claim(&s, Uuid::new_v4()).await.unwrap().is_none());
    sqlx::query("UPDATE alert_webhook_destinations SET enabled=false WHERE id=$1")
        .bind(d)
        .execute(&mut *management)
        .await
        .unwrap();
    management.commit().await.unwrap();
    assert_eq!(state(&pool, id).await.0, "ineligible");
    // Reverse direction: claim-time SHARE conflicts with management, but
    // queue INSERT's FK KEY SHARE still proceeds without waiting/deadlock.
    sqlx::query("UPDATE alert_webhook_destinations SET enabled=true WHERE id=$1")
        .bind(d)
        .execute(&pool)
        .await
        .unwrap();
    let mut gate = pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM alert_webhook_destinations WHERE id=$1 FOR SHARE")
        .bind(d)
        .execute(&mut *gate)
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), queue(&pool, d, false))
        .await
        .unwrap();
    let mut blocked = pool.begin().await.unwrap();
    sqlx::query("SET LOCAL lock_timeout='100ms'")
        .execute(&mut *blocked)
        .await
        .unwrap();
    assert!(
        sqlx::query("UPDATE alert_webhook_destinations SET enabled=false WHERE id=$1")
            .bind(d)
            .execute(&mut *blocked)
            .await
            .is_err()
    );
    blocked.rollback().await.unwrap();
    gate.commit().await.unwrap();
    let c = claimed(&s).await;
    complete(&s, &c, Ok(())).await.unwrap();
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn claimed_http_path_signatures_retry_redirect_and_unbounded_body(pool: PgPool) {
    use axum::{
        Router, body::Bytes, extract::State, http::HeaderMap, response::IntoResponse, routing::post,
    };
    use std::sync::{Arc, Mutex};
    type Seen = Arc<Mutex<Vec<(HeaderMap, Vec<u8>)>>>;
    let seen: Seen = Arc::default();
    async fn receiver(
        State(seen): State<Seen>,
        headers: HeaderMap,
        body: Bytes,
    ) -> impl IntoResponse {
        let n = {
            let mut seen = seen.lock().unwrap();
            seen.push((headers, body.to_vec()));
            seen.len()
        };
        if n == 1 {
            (axum::http::StatusCode::SERVICE_UNAVAILABLE, vec![])
        } else {
            (axum::http::StatusCode::OK, vec![b'x'; 1024 * 1024])
        }
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/hook", listener.local_addr().unwrap());
    let server = tokio::spawn(
        axum::serve(
            listener,
            Router::new()
                .route("/hook", post(receiver))
                .with_state(seen.clone()),
        )
        .into_future(),
    );
    let d: Uuid=sqlx::query_scalar("INSERT INTO alert_webhook_destinations(label,url,current_secret_ref,enabled) VALUES('receiver',$1,'env:HOOK_W45_ACCEPTANCE',true) RETURNING id").bind(&url).fetch_one(&pool).await.unwrap();
    let s = Store::new(pool.clone()).with_alert_webhooks(
        crate::config::AlertWebhookPolicy::parse(
            &[url],
            vec!["HOOK_W45_ACCEPTANCE".into()],
            crate::config::Environment::Development,
        )
        .unwrap(),
    );
    let id = queue(&pool, d, false).await;
    let first = claimed(&s).await;
    assert_eq!(
        send_with(&s, &first, |_| Some(zeroize::Zeroizing::new(
            b"01234567890123456789012345678901".to_vec()
        )))
        .await,
        Err(Failure::TransientStatus)
    );
    complete(&s, &first, Err(Failure::TransientStatus))
        .await
        .unwrap();
    sqlx::query("UPDATE alert_webhook_outbox SET next_attempt_at=clock_timestamp() WHERE id=$1")
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
    // Fresh Store simulates process restart; durable queue recovers unchanged bytes.
    let recovered = claimed(&s.clone()).await;
    let result = send_with(&s, &recovered, |_| {
        Some(zeroize::Zeroizing::new(
            b"01234567890123456789012345678901".to_vec(),
        ))
    })
    .await;
    assert_eq!(result, Ok(()));
    complete(&s, &recovered, result).await.unwrap();
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 2);
    for (headers, body) in seen.iter() {
        let header = headers["x-omg-signature"].to_str().unwrap();
        let t: i64 = header
            .strip_prefix("t=")
            .unwrap()
            .split(',')
            .next()
            .unwrap()
            .parse()
            .unwrap();
        // Independent Python/OpenSSL verifier is used by the running gateway
        // acceptance too; this checks the transport's exact bytes and id.
        assert_eq!(
            header,
            crate::audit_export::sign(b"01234567890123456789012345678901", t, body)
        );
        assert_eq!(headers["x-omg-delivery"].to_str().unwrap(), id.to_string());
        assert!(!String::from_utf8_lossy(body).contains("private canary"));
    }
    assert_eq!(seen[0].1, seen[1].1);
    server.abort();
}
