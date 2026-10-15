//! #75: placing, listing and releasing legal holds.
use super::*;

const HOLDS: &str = "/api/v1/platform/legal-holds";

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn holds_are_admin_placed_audited_listed_and_released_once(pool: PgPool) {
    let f = fixture(&pool).await;
    let user = json!({"user_id": f.outsider.user_id, "reference": "CASE-2026-0042"});
    for actor in [&f.auditor, &f.member, &f.outsider] {
        assert_eq!(
            call(&f.s, actor, "POST", HOLDS, user.clone()).await.0,
            StatusCode::FORBIDDEN
        );
    }
    let (status, placed) = call(&f.s, &f.admin, "POST", HOLDS, user.clone()).await;
    assert_eq!(status, StatusCode::CREATED, "{placed}");
    let id = placed["id"].as_str().unwrap().to_owned();
    assert_eq!(
        call(&f.s, &f.admin, "POST", HOLDS, user.clone()).await.0,
        StatusCode::CONFLICT,
        "same reference twice"
    );
    let lower = json!({"user_id": f.outsider.user_id, "reference": "case-2026-0042"});
    assert_eq!(
        call(&f.s, &f.admin, "POST", HOLDS, lower).await.0,
        StatusCode::CONFLICT,
        "references compare without case"
    );
    let team = json!({"workspace_id": f.team, "reference": "CASE-2026-0042"});
    assert_eq!(
        call(&f.s, &f.admin, "POST", HOLDS, team).await.0,
        StatusCode::CREATED
    );
    // Refused: a personal workspace (hold its owner), unknown targets, both or neither, free text.
    for (body, want) in [
        (
            json!({"workspace_id": f.personal, "reference": "X1"}),
            StatusCode::BAD_REQUEST,
        ),
        (
            json!({"user_id": Uuid::new_v4(), "reference": "X1"}),
            StatusCode::NOT_FOUND,
        ),
        (
            json!({"workspace_id": Uuid::new_v4(), "reference": "X1"}),
            StatusCode::NOT_FOUND,
        ),
        (
            json!({"user_id": f.owner.user_id, "workspace_id": f.team, "reference": "X1"}),
            StatusCode::BAD_REQUEST,
        ),
        (json!({"reference": "X1"}), StatusCode::BAD_REQUEST),
        (
            json!({"user_id": f.owner.user_id, "reference": "Free text with the person's name"}),
            StatusCode::BAD_REQUEST,
        ),
    ] {
        assert_eq!(
            call(&f.s, &f.admin, "POST", HOLDS, body.clone()).await.0,
            want,
            "{body}"
        );
    }
    // Admins and Auditors list; members don't.
    for actor in [&f.admin, &f.auditor] {
        let (status, list) = call(&f.s, actor, "GET", HOLDS, json!({})).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(list["data"].as_array().unwrap().len(), 2);
    }
    assert_eq!(
        call(&f.s, &f.member, "GET", HOLDS, json!({})).await.0,
        StatusCode::FORBIDDEN
    );
    // The user record shows a count only.
    let (_, detail) = call(
        &f.s,
        &f.admin,
        "GET",
        &format!("/api/v1/platform/users/{}", f.outsider.user_id),
        json!({}),
    )
    .await;
    assert_eq!(detail["legal_holds"], 1);
    // Released once; then 404; the list keeps it under state=all.
    let release = format!("{HOLDS}/{id}/release");
    assert_eq!(
        call(&f.s, &f.auditor, "POST", &release, json!({})).await.0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        call(&f.s, &f.admin, "POST", &release, json!({})).await.0,
        StatusCode::OK
    );
    assert_eq!(
        call(&f.s, &f.admin, "POST", &release, json!({})).await.0,
        StatusCode::CONFLICT,
        "already released"
    );
    assert_eq!(
        call(
            &f.s,
            &f.admin,
            "POST",
            &format!("{HOLDS}/{}/release", Uuid::new_v4()),
            json!({})
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    let (_, active) = call(&f.s, &f.admin, "GET", HOLDS, json!({})).await;
    assert_eq!(active["data"].as_array().unwrap().len(), 1);
    let (_, all) = call(
        &f.s,
        &f.admin,
        "GET",
        &format!("{HOLDS}?state=all"),
        json!({}),
    )
    .await;
    assert_eq!(all["data"].as_array().unwrap().len(), 2);
    let audited: Vec<(String, Value)> = sqlx::query_as("SELECT action,metadata FROM audit_events WHERE action LIKE 'legal_hold.%' ORDER BY created_at,id")
        .fetch_all(&pool).await.unwrap();
    assert_eq!(
        audited,
        vec![
            ("legal_hold.placed".into(), json!({"kind":"user"})),
            ("legal_hold.placed".into(), json!({"kind":"workspace"})),
            ("legal_hold.released".into(), json!({"kind":"user"})),
        ],
        "the reference stays on the hold, not in the audit metadata"
    );
    // A person whose data is already gone can't be held.
    sqlx::query("UPDATE users SET cleaned_at=now(),email=NULL WHERE id=$1")
        .bind(f.member.user_id)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        call(
            &f.s,
            &f.admin,
            "POST",
            HOLDS,
            json!({"user_id": f.member.user_id, "reference": "LATE"})
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
}
