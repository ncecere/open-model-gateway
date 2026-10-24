use super::*;

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn status_never_counts_late_stale_generation_as_waiting(pool: PgPool) {
    let f = fixture(&pool).await;
    let id:Uuid=sqlx::query_scalar("INSERT INTO alert_webhook_destinations(label,url,current_secret_ref,enabled) VALUES('receiver','https://example.test/hook','env:HOOK',true) RETURNING id").fetch_one(&pool).await.unwrap();
    let mut enqueue = pool.begin().await.unwrap();
    sqlx::query("INSERT INTO alert_webhook_outbox(id,destination_id,destination_generation,test,payload) SELECT i,$1,0,true,convert_to(alert_webhook_payload(i,NULL,NULL),'UTF8') FROM (SELECT gen_random_uuid() i) x").bind(id).execute(&mut *enqueue).await.unwrap();
    sqlx::query("UPDATE alert_webhook_destinations SET enabled=false WHERE id=$1")
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE alert_webhook_destinations SET enabled=true WHERE id=$1")
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
    enqueue.commit().await.unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM alert_webhook_outbox WHERE state='waiting'"
        )
        .fetch_one(&pool)
        .await
        .unwrap(),
        1
    );
    let (_, item) = call(
        &f.s,
        &f.auditor,
        "GET",
        &format!("{WEBHOOKS}/{id}"),
        json!({}),
    )
    .await;
    assert_eq!(item["waiting"], 0);
    let (_, status) = call(
        &f.s,
        &f.auditor,
        "GET",
        &format!("{WEBHOOKS}/{id}/deliveries"),
        json!({}),
    )
    .await;
    assert_eq!(status["counts"]["waiting"], 0);
    assert_eq!(status["data"].as_array().unwrap().len(), 0);
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn saved_tests_limits_redaction_and_strict_body(pool: PgPool) {
    let f = fixture(&pool).await;
    let id: Uuid=sqlx::query_scalar("INSERT INTO alert_webhook_destinations(label,url,current_secret_ref) VALUES('receiver','https://example.test/private-canary','env:PRIVATE_CANARY') RETURNING id").fetch_one(&pool).await.unwrap();
    let path = format!("{WEBHOOKS}/{id}/test");
    assert_eq!(
        call(&f.s, &f.admin, "POST", &path, json!({})).await.0,
        StatusCode::CONFLICT
    );
    sqlx::query("UPDATE alert_webhook_destinations SET enabled=true WHERE id=$1")
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
    for body in [
        json!({"url":"https://example.test"}),
        json!({"event_id":Uuid::new_v4()}),
        json!([]),
        Value::Null,
        json!({"headers":{}}),
    ] {
        assert_eq!(
            call(&f.s, &f.admin, "POST", &path, body).await.0,
            StatusCode::BAD_REQUEST
        );
    }
    for u in [&f.member, &f.owner, &f.auditor] {
        assert_eq!(
            call(&f.s, u, "POST", &path, json!({})).await.0,
            StatusCode::FORBIDDEN
        );
    }
    let replica_a = f.s.clone();
    let replica_b = f.s.clone();
    let (one, two) = tokio::join!(
        call(&replica_a, &f.admin, "POST", &path, json!({})),
        call(&replica_b, &f.admin, "POST", &path, json!({}))
    );
    assert_eq!(one.0, StatusCode::OK);
    assert_eq!(two.0, StatusCode::OK);
    assert_ne!(one.1["id"], two.1["id"]);
    assert_eq!(
        call(&f.s, &f.admin, "POST", &path, json!({})).await.0,
        StatusCode::TOO_MANY_REQUESTS
    );
    let read = format!("{WEBHOOKS}/{id}/deliveries");
    let (code, value) = call(&f.s, &f.auditor, "GET", &read, json!({})).await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(value["counts"]["waiting"], 2);
    assert_eq!(value["data"].as_array().unwrap().len(), 2);
    let text = value.to_string();
    assert!(!text.contains("private-canary"));
    assert!(!text.contains("PRIVATE_CANARY"));
    assert!(!text.contains("payload"));
    let count:i64=sqlx::query_scalar("SELECT count(*) FROM audit_events WHERE action='alert_webhook.tested' AND actor_user_id=$1 AND resource_id=$2").bind(f.admin.user_id).bind(id).fetch_one(&pool).await.unwrap();
    assert_eq!(count, 2);
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn test_limits_destination_and_installation_across_actors(pool: PgPool) {
    let f = fixture(&pool).await;
    let other = user(&pool, "second-webhook-admin", "admin").await;
    let id: Uuid=sqlx::query_scalar("INSERT INTO alert_webhook_destinations(label,url,current_secret_ref,enabled) VALUES('receiver','https://example.test/path','env:HOOK',true) RETURNING id").fetch_one(&pool).await.unwrap();
    let path = format!("{WEBHOOKS}/{id}/test");
    for _ in 0..2 {
        assert_eq!(
            call(&f.s, &f.admin, "POST", &path, json!({})).await.0,
            StatusCode::OK
        );
    }
    assert_eq!(
        call(&f.s, &other, "POST", &path, json!({})).await.0,
        StatusCode::TOO_MANY_REQUESTS
    );
    // Count earlier tests (including failures) toward the shared hourly cap.
    for _ in 0..18 {
        sqlx::query("INSERT INTO audit_events(id,action,resource_type,resource_id) VALUES(gen_random_uuid(),'alert_webhook.tested','alert_webhook',gen_random_uuid())").execute(&pool).await.unwrap();
    }
    let another: Uuid=sqlx::query_scalar("INSERT INTO alert_webhook_destinations(label,url,current_secret_ref,enabled) VALUES('other','https://example.test/other','env:HOOK',true) RETURNING id").fetch_one(&pool).await.unwrap();
    assert_eq!(
        call(
            &f.s,
            &other,
            "POST",
            &format!("{WEBHOOKS}/{another}/test"),
            json!({})
        )
        .await
        .0,
        StatusCode::TOO_MANY_REQUESTS
    );
}
