//! #428: a model or catalog description with a control character reached
//! PostgreSQL (NUL: SQLSTATE 22021, surfaced as 503). Descriptions are
//! multi-line text, so tab, line feed and carriage return stay accepted; every
//! other control character is refused with 400 on create and PATCH.
use super::*;

const REFUSED: [&str; 7] = [
    "\u{0}",
    "a\u{0}b",
    "\u{7}",
    "\u{1b}[31m",
    "\u{7f}",
    "\u{85}",
    "\u{9f}",
];

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn descriptions_refuse_control_characters(pool: PgPool) {
    let f = fixture(&pool).await;
    let model = |name: &str, description: &str| json!({"public_name":name,"display_name":"Described","description":description,"enabled":false,"supported_protocols":["chat_completions"]});
    let (status, created) = call(
        &f.s,
        &f.admin,
        "POST",
        "/api/v1/platform/models",
        model("described/base", "plain"),
    )
    .await;
    assert!(status.is_success(), "model create: {status}");
    let model_id = created["id"].as_str().unwrap().to_owned();
    let (status, created) = call(
        &f.s,
        &f.admin,
        "POST",
        "/api/v1/platform/catalogs",
        json!({"name":"Described","description":"plain"}),
    )
    .await;
    assert!(status.is_success(), "catalog create: {status}");
    let catalog_id = created["id"].as_str().unwrap().to_owned();
    // Collect every case (labels and statuses only, never the value).
    let mut wrong = Vec::new();
    for (index, description) in REFUSED.iter().enumerate() {
        for (label, method, path, body) in [
            (
                "model create",
                "POST",
                "/api/v1/platform/models".to_owned(),
                model(&format!("described/refused-{index}"), description),
            ),
            (
                "model patch",
                "PATCH",
                format!("/api/v1/platform/models/{model_id}"),
                json!({"description":description}),
            ),
            (
                "catalog create",
                "POST",
                "/api/v1/platform/catalogs".to_owned(),
                json!({"name":format!("Refused {index}"),"description":description}),
            ),
            (
                "catalog patch",
                "PATCH",
                format!("/api/v1/platform/catalogs/{catalog_id}"),
                json!({"name":"Described","description":description}),
            ),
        ] {
            let (status, body) = call(&f.s, &f.admin, method, &path, body).await;
            if status != StatusCode::BAD_REQUEST || body["error"]["code"] != "400" {
                wrong.push(format!("{label} #{index}: {status}"));
            }
        }
    }
    assert!(
        wrong.is_empty(),
        "not refused with 400:\n{}",
        wrong.join("\n")
    );
    // Nothing was stored.
    let stored: Vec<Option<String>> = sqlx::query_scalar(
        "SELECT description FROM models WHERE id=$1::uuid UNION ALL SELECT description FROM catalogs WHERE id=$2::uuid",
    )
    .bind(&model_id)
    .bind(&catalog_id)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        stored,
        vec![Some("plain".to_owned()), Some("plain".to_owned())]
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn descriptions_keep_multiline_text(pool: PgPool) {
    let f = fixture(&pool).await;
    let text = "First line\nsecond line\twith a tab\r\nand a 2000-byte limit: caf\u{e9}";
    let (status, model) = call(
        &f.s,
        &f.admin,
        "POST",
        "/api/v1/platform/models",
        json!({"public_name":"described/multiline","display_name":"Multiline","description":text,"enabled":false,"supported_protocols":["chat_completions"]}),
    )
    .await;
    assert!(status.is_success(), "model create: {status}");
    let (status, _) = call(
        &f.s,
        &f.admin,
        "PATCH",
        &format!("/api/v1/platform/models/{}", model["id"].as_str().unwrap()),
        json!({"description":text}),
    )
    .await;
    assert!(status.is_success(), "model patch: {status}");
    let (status, _) = call(
        &f.s,
        &f.admin,
        "POST",
        "/api/v1/platform/catalogs",
        json!({"name":"Multiline","description":text}),
    )
    .await;
    assert!(status.is_success(), "catalog create: {status}");
    // The existing length limit is unchanged.
    let (status, _) = call(
        &f.s,
        &f.admin,
        "POST",
        "/api/v1/platform/catalogs",
        json!({"name":"Too long","description":"x".repeat(2001)}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}
