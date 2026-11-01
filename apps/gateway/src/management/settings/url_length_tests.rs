//! Stored, percent-encoded URL length boundaries through the real general route.
use super::*;

async fn check_url_length(pool: PgPool, field: &str) {
    let f = fixture(&pool).await;
    let path = "/api/v1/platform/settings/general";
    let prefix = "https://support.example.test/";
    for (tail, expected) in [("aaa", StatusCode::OK), ("aaaa", StatusCode::BAD_REQUEST)] {
        let raw = format!("{prefix}{}{tail}", "é".repeat(336));
        let normalized = reqwest::Url::parse(&raw).unwrap().to_string();
        assert!(
            normalized.len() == 2045 + tail.len(),
            "{field}: boundary setup"
        );
        let mut body = json!({"display_name":"Acme AI","human_key_max_lifetime_days":30});
        body[field] = json!(raw);
        let (status, response) = call(&f.s, &f.admin, "PUT", path, body).await;
        assert_eq!(status, expected, "{field}: encoded length boundary");
        if expected == StatusCode::OK {
            assert!(
                response[field].as_str() == Some(normalized.as_str()),
                "{field}: normalized response"
            );
            let (_, stored) = call(&f.s, &f.admin, "GET", path, json!({})).await;
            assert!(
                stored[field].as_str() == Some(normalized.as_str()),
                "{field}: persisted URL"
            );
        } else {
            assert!(
                response["error"]["message"] == "Invalid request",
                "{field}: invalid error"
            );
        }
    }
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn support_url_encoded_length_boundary(pool: PgPool) {
    check_url_length(pool, "support_url").await;
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn logo_url_encoded_length_boundary(pool: PgPool) {
    check_url_length(pool, "logo_url").await;
}
