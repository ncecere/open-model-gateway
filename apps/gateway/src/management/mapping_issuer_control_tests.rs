//! #427: an OIDC group-mapping issuer containing a control character or
//! whitespace is refused with 400 before it can reach PostgreSQL (which rejects NUL with
//! SQLSTATE 22021, surfaced as 503). The URL parser strips leading/trailing C0
//! controls and percent-encodes them in the path, so URL validation alone let
//! them through while the raw issuer was stored.
use super::*;

const ISSUER: &str = "https://idp.example.test";

fn mapping(issuer: &str) -> Value {
    json!({"issuer":issuer,"group_value":"control-test","target_kind":"platform","platform_role":"user","workspace_id":null,"workspace_role":null,"enabled":false})
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn mapping_issuer_refuses_control_characters(pool: PgPool) {
    let f = fixture(&pool).await;
    let (status, created) = call(
        &f.s,
        &f.admin,
        "POST",
        "/api/v1/platform/oidc/group-mappings",
        mapping(ISSUER),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{created}");
    let existing = id(&created);
    let issuers = [
        format!("\u{0}{ISSUER}/"),
        format!("{ISSUER}/\u{0}"),
        format!("{ISSUER}/a\u{0}b"),
        format!("\n{ISSUER}/"),
        format!("{ISSUER}/a\u{7}b"),
        format!("{ISSUER}/a\u{85}b"),
        format!("{ISSUER}/a\u{7f}b"),
        format!("{ISSUER}\t/"),
        format!(" {ISSUER}/"),
        format!("{ISSUER}/ "),
        format!("{ISSUER}/a b"),
        format!("{ISSUER}/a\u{a0}b"),
        format!("{ISSUER}/a\u{2028}b"),
    ];
    // Collect every case so one run reports all of them (labels only, never values).
    let mut wrong = Vec::new();
    for (index, issuer) in issuers.iter().enumerate() {
        for (route, method, path, body) in [
            (
                "create",
                "POST",
                "/api/v1/platform/oidc/group-mappings".to_owned(),
                mapping(issuer),
            ),
            (
                "new preview",
                "POST",
                "/api/v1/platform/oidc/group-mappings/preview".to_owned(),
                mapping(issuer),
            ),
            (
                "patch",
                "PATCH",
                format!("/api/v1/platform/oidc/group-mappings/{existing}"),
                json!({"issuer":issuer}),
            ),
            (
                "existing preview",
                "POST",
                format!("/api/v1/platform/oidc/group-mappings/{existing}/preview"),
                json!({"issuer":issuer}),
            ),
        ] {
            let (status, _) = call(&f.s, &f.admin, method, &path, body).await;
            if status != StatusCode::BAD_REQUEST {
                wrong.push(format!("{route} issuer #{index}: {status}"));
            }
        }
    }
    assert!(
        wrong.is_empty(),
        "not refused with 400:\n{}",
        wrong.join("\n")
    );
    // Ordinary issuers are unchanged, and the stored mapping was never altered.
    for issuer in [ISSUER.to_owned(), format!("{ISSUER}/tenant/a")] {
        let (status, _) = call(
            &f.s,
            &f.admin,
            "POST",
            "/api/v1/platform/oidc/group-mappings/preview",
            mapping(&issuer),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }
    let stored: String = sqlx::query_scalar("SELECT issuer FROM oidc_group_mappings WHERE id=$1")
        .bind(existing)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(stored == ISSUER, "the stored issuer must be unchanged");
}

/// A row stored before #427 with a control character in its issuer (it could
/// never match a real OIDC `iss`): a PATCH of another field now answers 400,
/// because the merged mapping is validated, and DELETE still removes it.
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn legacy_control_character_issuer_can_be_deleted_not_patched(pool: PgPool) {
    let f = fixture(&pool).await;
    let legacy = Uuid::new_v4();
    sqlx::query("INSERT INTO oidc_group_mappings(id,issuer,group_value,target_kind,platform_role,enabled) VALUES($1,$2,'legacy','platform','user',true)")
        .bind(legacy)
        .bind(format!("{ISSUER}/\n"))
        .execute(&pool)
        .await
        .unwrap();
    let path = format!("/api/v1/platform/oidc/group-mappings/{legacy}");
    let (status, body) = call(&f.s, &f.admin, "PATCH", &path, json!({"enabled":false})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        !body.to_string().contains(ISSUER),
        "the refusal never echoes the stored issuer"
    );
    let (status, _) = call(&f.s, &f.admin, "DELETE", &path, Value::Null).await;
    assert!(status.is_success(), "a legacy row can still be deleted");
}
