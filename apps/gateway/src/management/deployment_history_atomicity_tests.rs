//! #86 regression checks for atomic capture and timestamp/ID cursor ties.
use super::*;

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn deployment_history_capture_failure_rolls_back_enablement_and_revision(pool: PgPool) {
    let f = fixture(&pool).await;
    let m = model(&pool, "history-atomic").await;
    let p = Uuid::new_v4();
    let d = Uuid::new_v4();
    sqlx::query("INSERT INTO provider_connections(id,name,provider,credential_ref) VALUES($1,'Atomic history','openai','env:UNUSED')").bind(p).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO deployments(id,model_id,provider_connection_id,upstream_model) VALUES($1,$2,$3,'atomic')").bind(d).bind(m).bind(p).execute(&pool).await.unwrap();
    sqlx::raw_sql("CREATE FUNCTION reject_configuration_audit() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.action='configuration.updated' THEN RAISE EXCEPTION 'test rejection'; END IF; RETURN NEW; END $$; CREATE TRIGGER reject_configuration_audit BEFORE INSERT ON audit_events FOR EACH ROW EXECUTE FUNCTION reject_configuration_audit()")
        .execute(&pool).await.unwrap();
    let (status, _) = call(
        &f.s,
        &f.admin,
        "PATCH",
        &format!("/api/v1/platform/deployments/{d}"),
        json!({"enabled":true}),
    )
    .await;
    assert!(!status.is_success());
    let state: (bool, i64) = sqlx::query_as("SELECT enabled,revision FROM deployments WHERE id=$1")
        .bind(d)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(state, (false, 1));
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM configuration_changes WHERE deployment_id=$1"
        )
        .bind(d)
        .fetch_one(&pool)
        .await
        .unwrap(),
        1
    );
    // Snapshot insertion failure is equally atomic, including its preceding audit.
    sqlx::raw_sql("DROP TRIGGER reject_configuration_audit ON audit_events; CREATE FUNCTION reject_configuration_snapshot() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'test rejection'; END $$; CREATE TRIGGER reject_configuration_snapshot BEFORE INSERT ON configuration_changes FOR EACH ROW EXECUTE FUNCTION reject_configuration_snapshot()")
        .execute(&pool).await.unwrap();
    assert!(
        sqlx::query("UPDATE deployments SET enabled=true WHERE id=$1")
            .bind(d)
            .execute(&pool)
            .await
            .is_err()
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT revision FROM deployments WHERE id=$1")
            .bind(d)
            .fetch_one(&pool)
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM audit_events WHERE resource_id=$1")
            .bind(d)
            .fetch_one(&pool)
            .await
            .unwrap(),
        1
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn deployment_history_cursor_ties_do_not_skip_or_repeat_changes(pool: PgPool) {
    let f = fixture(&pool).await;
    let target = Uuid::new_v4();
    let path = format!("/api/v1/platform/deployments/{target}/configuration/history?limit=1");
    // Deliberately absent target: history is keyed by immutable ID, not names.
    let ids = [Uuid::from_u128(1), Uuid::from_u128(2), Uuid::from_u128(3)];
    for id in ids {
        sqlx::query("INSERT INTO audit_events(id,created_at,action,resource_type,resource_id) VALUES($1,date_trunc('day',now()),'configuration.updated','deployments',$2)").bind(id).bind(target).execute(&pool).await.unwrap();
    }
    let mut cursor: Option<String> = None;
    for expected in ids.into_iter().rev() {
        let url = format!(
            "{path}{}",
            cursor
                .as_ref()
                .map(|c| format!("&cursor={c}"))
                .unwrap_or_default()
        );
        let (status, page) = call(&f.s, &f.auditor, "GET", &url, Value::Null).await;
        assert_eq!(status, StatusCode::OK, "{page}");
        assert_eq!(page["data"].as_array().unwrap().len(), 1);
        assert_eq!(page["data"][0]["id"], expected.to_string());
        cursor = page["next_cursor"].as_str().map(str::to_owned);
    }
    assert!(cursor.is_none());
}
