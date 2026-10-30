//! Aged-hold incidents use live authority on every read path, including history.
use super::*;

async fn incident(pool: &PgPool, ws: Option<Uuid>) -> Uuid {
    let event = Uuid::new_v4();
    sqlx::query("INSERT INTO alert_events(id,kind,subject_key,workspace_id,level,severity,summary,details) VALUES($1,'aged_unknown_hold',$2,$3,1,'warning','Holds with unknown cost are over 24 hours old','{\"threshold_hours\":24}')")
        .bind(event).bind(ws.map_or_else(|| "personal".into(), |ws| format!("workspace:{ws}"))).bind(ws).execute(pool).await.unwrap();
    // As `aged_hold_tick` writes it: the fired email comes with the incident.
    sqlx::query(
        "INSERT INTO alert_deliveries(id,event_id,transition) VALUES(gen_random_uuid(),$1,'fired')",
    )
    .bind(event)
    .execute(pool)
    .await
    .unwrap();
    event
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn aged_hold_visibility_feed_summary_read_history_and_old_replicas(pool: PgPool) {
    let f = fixture(&pool).await;
    let shared = incident(&pool, Some(f.team)).await;
    let personal = incident(&pool, None).await;
    for (user, expected) in [
        (&f.admin, 2),
        (&f.auditor, 2),
        (&f.owner, 1),
        (&f.member, 0),
        (&f.outsider, 0),
    ] {
        assert_eq!(feed(&f, user).await.len(), expected);
        assert_eq!(unread(&f, user).await, expected as i64);
        let (_, marked) = call(
            &f.s,
            user,
            "POST",
            "/api/v1/me/notifications/read",
            json!({"ids":[shared,personal]}),
        )
        .await;
        assert_eq!(marked["marked"], expected);
        assert_eq!(unread(&f, user).await, 0);
        // Actual pre-kind predicates: every old generic branch requires a rule or builtin.
        let old: i64 = sqlx::query_scalar(&format!(
            "SELECT count(*) {EVENT_FROM} WHERE {PRE_0049_VISIBLE}"
        ))
        .bind(user.user_id)
        .bind(true)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(old, 0);
    }
    for user in [&f.admin, &f.auditor] {
        let (status, history) = call(
            &f.s,
            user,
            "GET",
            "/api/v1/platform/alerts/events?kind=aged_unknown_hold",
            json!({}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(history["data"].as_array().unwrap().len(), 2);
        assert!(
            history["data"]
                .as_array()
                .unwrap()
                .iter()
                .find(|e| e["id"] == personal.to_string())
                .unwrap()["workspace"]
                .is_null()
        );
    }
    // Resolve once, then revoke membership: old attribution/read marks grant nothing.
    sqlx::query("UPDATE alert_events SET resolved_at=clock_timestamp(),resolution='cleared'")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE workspace_membership_grants SET revoked_at=clock_timestamp() WHERE workspace_id=$1 AND user_id=$2")
        .bind(f.team).bind(f.owner.user_id).execute(&pool).await.unwrap();
    assert!(feed(&f, &f.owner).await.is_empty());
    assert_eq!(feed(&f, &f.admin).await.len(), 2);
    let (status, result) = call(
        &f.s,
        &f.admin,
        "POST",
        RULES,
        json!({"name":"not a rule","kind":"aged_unknown_hold"}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{result}");
    let old_history: i64 = sqlx::query_scalar(&format!("SELECT count(*) {EVENT_FROM} WHERE r.scope='installation' OR e.builtin IN ('scim_last_admin','partitions_missing','history_orphans') OR r.scope='workspace' OR e.builtin IS NOT NULL"))
        .fetch_one(&pool).await.unwrap();
    assert_eq!(old_history, 0);
}
