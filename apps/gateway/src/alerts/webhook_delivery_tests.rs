use super::*;
use sqlx::PgPool;

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn lifetime_cutoffs_and_policy_failures(pool: PgPool) {
    let d = destination(&pool).await;
    let s = Store::new(pool.clone());
    // Fixtures can set INSERT creation time as owner; runtime cannot.
    let event: Uuid=sqlx::query_scalar("INSERT INTO alert_events(id,builtin,kind,subject_key,level,severity,summary) VALUES(gen_random_uuid(),'scim_last_admin','scim_last_admin',gen_random_uuid()::text,1,'critical','safe') RETURNING id").fetch_one(&pool).await.unwrap();
    let id: Uuid=sqlx::query_scalar("INSERT INTO alert_webhook_outbox(id,destination_id,destination_generation,event_id,transition,payload,created_at) SELECT i,$1,0,$2,'fired',convert_to(alert_webhook_payload(i,$2,'fired'),'UTF8'),clock_timestamp()-interval '24 hours'+interval '45 seconds' FROM (SELECT gen_random_uuid() i) x RETURNING id").bind(d).bind(event).fetch_one(&pool).await.unwrap();
    let c = claimed(&s).await;
    assert!(complete(&s, &c, Err(Failure::Connection)).await.unwrap());
    assert_eq!(
        state(&pool, id).await.0,
        "exhausted",
        "retry at +60s is beyond cutoff"
    );
    for age in ["24 hours", "25 hours"] {
        let id: Uuid=sqlx::query_scalar("INSERT INTO alert_webhook_outbox(id,destination_id,destination_generation,test,payload,created_at) SELECT i,$1,0,true,convert_to(alert_webhook_payload(i,NULL,NULL),'UTF8'),clock_timestamp()-$2::interval FROM (SELECT gen_random_uuid() i) x RETURNING id").bind(d).bind(age).fetch_one(&pool).await.unwrap();
        assert!(matches!(
            claim(&s, Uuid::new_v4()).await.unwrap(),
            Some(None)
        ));
        assert_eq!(
            state(&pool, id).await,
            ("exhausted".into(), 0, Some("interrupted".into()))
        );
    }
    for url in [
        "https://10.0.0.1/hook",
        "https://169.254.169.254/hook",
        "https://localhost/hook",
    ] {
        let d: Uuid=sqlx::query_scalar("INSERT INTO alert_webhook_destinations(label,url,current_secret_ref,enabled) VALUES('receiver',$1,'env:HOOK',true) RETURNING id").bind(url).fetch_one(&pool).await.unwrap();
        let id = queue(&pool, d, true).await;
        let s = Store::new(pool.clone()).with_alert_webhooks(
            crate::config::AlertWebhookPolicy::parse(
                &[url.into()],
                vec!["HOOK".into()],
                crate::config::Environment::Production,
            )
            .unwrap(),
        );
        let c = claimed(&s).await;
        let result = send_with(&s, &c, |_| Some(zeroize::Zeroizing::new(vec![b's'; 32]))).await;
        assert_eq!(result, Err(Failure::HostNotAllowed), "{url}");
        complete(&s, &c, result).await.unwrap();
        assert_eq!(state(&pool, id).await.0, "failed");
    }
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn tls_and_connection_close_have_distinct_terminal_policy(pool: PgPool) {
    for (tls, expected) in [(false, Failure::Connection), (true, Failure::Tls)] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!(
            "{}://{}/hook",
            if tls { "https" } else { "http" },
            listener.local_addr().unwrap()
        );
        let receiver = tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = [0; 8192];
            let _ = socket.read(&mut buf).await;
            if tls {
                socket
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
                    .await
                    .unwrap();
            }
        });
        let d:Uuid=sqlx::query_scalar("INSERT INTO alert_webhook_destinations(label,url,current_secret_ref,enabled) VALUES('receiver',$1,'env:HOOK',true) RETURNING id").bind(&url).fetch_one(&pool).await.unwrap();
        queue(&pool, d, true).await;
        let s = Store::new(pool.clone()).with_alert_webhooks(
            crate::config::AlertWebhookPolicy::parse(
                &[url],
                vec!["HOOK".into()],
                crate::config::Environment::Development,
            )
            .unwrap(),
        );
        let c = claimed(&s).await;
        assert_eq!(
            send_with(&s, &c, |_| Some(zeroize::Zeroizing::new(vec![b's'; 32]))).await,
            Err(expected)
        );
        // Release this claim so the next fixture is the only due row.
        complete(&s, &c, Err(expected)).await.unwrap();
        receiver.await.unwrap();
    }
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn dispatch_is_bounded_before_claiming(pool: PgPool) {
    let d = destination(&pool).await;
    let s = Store::new(pool.clone());
    for _ in 0..26 {
        queue(&pool, d, true).await;
    }
    deliver_within(&s, Duration::from_secs(1)).await;
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM alert_webhook_outbox WHERE state='waiting' AND attempt_count=0"
        )
        .fetch_one(&pool)
        .await
        .unwrap(),
        26
    );
    deliver(&s).await;
    assert_eq!(sqlx::query_scalar::<_,i64>("SELECT count(*) FROM alert_webhook_outbox WHERE state='failed' AND attempt_count=1 AND last_error='destination_policy'").fetch_one(&pool).await.unwrap(),25);
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM alert_webhook_outbox WHERE state='waiting' AND attempt_count=0"
        )
        .fetch_one(&pool)
        .await
        .unwrap(),
        1
    );
}

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
async fn rule_notification_revocation_blocks_initial_claim_and_retry(pool: PgPool) {
    use axum::{Router, routing::post};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = calls.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/hook", listener.local_addr().unwrap());
    let server = tokio::spawn(
        axum::serve(
            listener,
            Router::new().route(
                "/hook",
                post(move || {
                    let seen = seen.clone();
                    async move {
                        seen.fetch_add(1, Ordering::SeqCst);
                        axum::http::StatusCode::SERVICE_UNAVAILABLE
                    }
                }),
            ),
        )
        .into_future(),
    );
    let d: Uuid = sqlx::query_scalar("INSERT INTO alert_webhook_destinations(label,url,current_secret_ref,enabled) VALUES('receiver',$1,'env:HOOK',true) RETURNING id").bind(&url).fetch_one(&pool).await.unwrap();
    let s = Store::new(pool.clone()).with_alert_webhooks(
        crate::config::AlertWebhookPolicy::parse(
            &[url],
            vec!["HOOK".into()],
            crate::config::Environment::Development,
        )
        .unwrap(),
    );
    for retry in [false, true] {
        let rule: Uuid = sqlx::query_scalar("INSERT INTO alert_rules(id,scope,kind,name,spike_factor_percent,min_spend_microusd,notify_platform_admins) VALUES(gen_random_uuid(),'installation','spend_spike','rule',200,1,true) RETURNING id").fetch_one(&pool).await.unwrap();
        let event: Uuid = sqlx::query_scalar("INSERT INTO alert_events(id,rule_id,kind,subject_key,level,severity,summary) VALUES(gen_random_uuid(),$1,'spend_spike',gen_random_uuid()::text,1,'warning','private canary') RETURNING id").bind(rule).fetch_one(&pool).await.unwrap();
        // Exercise the evaluator's real enqueue trigger, not a direct test row.
        sqlx::query("INSERT INTO alert_deliveries(id,event_id,transition) VALUES(gen_random_uuid(),$1,'fired')").bind(event).execute(&pool).await.unwrap();
        let id: Uuid = sqlx::query_scalar(
            "SELECT id FROM alert_webhook_outbox WHERE destination_id=$1 AND event_id=$2",
        )
        .bind(d)
        .bind(event)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(state(&pool, id).await, ("waiting".into(), 0, None));
        if retry {
            let c = claimed(&s).await;
            assert_eq!(c.row.id, id);
            let result = send_with(&s, &c, |_| Some(zeroize::Zeroizing::new(vec![b's'; 32]))).await;
            assert_eq!(result, Err(Failure::TransientStatus));
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            assert!(complete(&s, &c, result).await.unwrap());
            sqlx::query(
                "UPDATE alert_webhook_outbox SET next_attempt_at=clock_timestamp() WHERE id=$1",
            )
            .bind(id)
            .execute(&pool)
            .await
            .unwrap();
        }
        sqlx::query("UPDATE alert_rules SET notify_platform_admins=false WHERE id=$1")
            .bind(rule)
            .execute(&pool)
            .await
            .unwrap();
        let before = calls.load(Ordering::SeqCst);
        deliver(&s).await;
        let (status, attempts, _) = state(&pool, id).await;
        assert_eq!(status, "ineligible");
        assert_eq!(attempts, i32::from(retry));
        assert_eq!(
            calls.load(Ordering::SeqCst),
            before,
            "revoked rule reached receiver"
        );
        assert!(claim(&s, Uuid::new_v4()).await.unwrap().is_none());
    }
    server.abort();
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
