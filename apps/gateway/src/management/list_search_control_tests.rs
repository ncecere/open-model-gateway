//! #404: a control character in a list's `q` is refused with 400 before it can
//! reach PostgreSQL (which rejects NUL with SQLSTATE 22021, surfaced as 503).
use super::*;

/// Every list that filters on `q` through the shared search helper or its own
/// check. Each is reached through the management router with a real URI, so
/// the query string is percent-decoded by the production extractor.
fn lists(f: &Fixture) -> Vec<(String, &BrowserPrincipal)> {
    vec![
        ("/api/v1/platform/workspaces".to_owned(), &f.admin),
        ("/api/v1/platform/users".to_owned(), &f.admin),
        ("/api/v1/platform/providers".to_owned(), &f.admin),
        ("/api/v1/platform/deployments".to_owned(), &f.admin),
        ("/api/v1/platform/models".to_owned(), &f.admin),
        (format!("/api/v1/workspaces/{}/catalog", f.team), &f.owner),
        ("/api/v1/platform/catalogs".to_owned(), &f.admin),
        (format!("/api/v1/workspaces/{}/models", f.team), &f.owner),
        (
            format!("/api/v1/workspaces/{}/available-models", f.team),
            &f.owner,
        ),
    ]
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn list_search_refuses_control_characters(pool: PgPool) {
    let f = fixture(&pool).await;
    // Collect every case so one run reports all the routes, not just the first.
    let mut wrong = Vec::new();
    for (path, who) in lists(&f) {
        for q in ["%00", "%07", "a%0Ab", "a%00", "%00a", "a%7Fb"] {
            let (status, body) =
                call(&f.s, who, "GET", &format!("{path}?q={q}"), Value::Null).await;
            let envelope =
                body["error"]["code"] == "400" && body["error"]["message"] == "Invalid request";
            if status != StatusCode::BAD_REQUEST || !envelope {
                wrong.push(format!("{path} q={q}: {status} {body}"));
            }
        }
    }
    assert!(
        wrong.is_empty(),
        "not refused with 400:\n{}",
        wrong.join("\n")
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn list_search_still_accepts_ordinary_queries(pool: PgPool) {
    let f = fixture(&pool).await;
    for (path, who) in lists(&f) {
        // An ordinary search, a blank one, a 200-character one and an
        // accented one are all unchanged; 201 characters is still refused.
        for q in [
            "team".to_owned(),
            "%20".to_owned(),
            "x".repeat(200),
            "caf%C3%A9".to_owned(),
        ] {
            let (status, body) =
                call(&f.s, who, "GET", &format!("{path}?q={q}"), Value::Null).await;
            assert_eq!(status, StatusCode::OK, "{path} q={q}: {body}");
        }
        let (status, _) = call(
            &f.s,
            who,
            "GET",
            &format!("{path}?q={}", "x".repeat(201)),
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{path}");
    }
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn list_search_ordinary_queries_still_match(pool: PgPool) {
    let f = fixture(&pool).await;
    let (status, found) = call(
        &f.s,
        &f.admin,
        "GET",
        "/api/v1/platform/workspaces?q=TEAM",
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{found}");
    assert_eq!(found["data"].as_array().unwrap().len(), 1, "{found}");
    let (status, found) = call(
        &f.s,
        &f.admin,
        "GET",
        "/api/v1/platform/users?q=OWNER",
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{found}");
    assert_eq!(found["data"].as_array().unwrap().len(), 1, "{found}");
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn file_list_search_refuses_control_characters(pool: PgPool) {
    let f = fixture(&pool).await;
    let path = format!("/api/v1/workspaces/{}/files", f.team);
    let mut wrong = Vec::new();
    for q in ["%00", "%07", "a%0Ab", "a%00", "a%C2%85b"] {
        let (status, _) = call(
            &f.s,
            &f.owner,
            "GET",
            &format!("{path}?search={q}"),
            Value::Null,
        )
        .await;
        if status != StatusCode::BAD_REQUEST {
            wrong.push(format!("files search={q}: {status}"));
        }
    }
    // Ordinary, blank and 120-character searches are unchanged; 121 is refused.
    for (q, expected) in [
        ("report".to_owned(), StatusCode::OK),
        ("%20".to_owned(), StatusCode::OK),
        ("x".repeat(120), StatusCode::OK),
        ("x".repeat(121), StatusCode::BAD_REQUEST),
    ] {
        let (status, _) = call(
            &f.s,
            &f.owner,
            "GET",
            &format!("{path}?search={q}"),
            Value::Null,
        )
        .await;
        if status != expected {
            wrong.push(format!("files search length {}: {status}", q.len()));
        }
    }
    assert!(wrong.is_empty(), "files search:\n{}", wrong.join("\n"));
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn list_search_edge_inputs(pool: PgPool) {
    let f = fixture(&pool).await;
    let mut wrong = Vec::new();
    for (path, who) in lists(&f) {
        // An inner C1 control (NEL) is a control character too. (A lone NEL is
        // Unicode whitespace, so the trimming search() reads it as no filter.)
        let (status, _) = call(&f.s, who, "GET", &format!("{path}?q=a%C2%85b"), Value::Null).await;
        if status != StatusCode::BAD_REQUEST {
            wrong.push(format!("{path} C1: {status}"));
        }
        // Invalid UTF-8 decodes to U+FFFD (not a control character): never a 5xx.
        for q in ["%C0%80", "%ED%A0%80", "%FF"] {
            let (status, _) = call(&f.s, who, "GET", &format!("{path}?q={q}"), Value::Null).await;
            if status != StatusCode::OK {
                wrong.push(format!("{path} q={q}: {status}"));
            }
        }
    }
    // search() trims first: surrounding whitespace (even a newline) is not a
    // control character inside the query, and a newline alone means no filter.
    for path in [
        "/api/v1/platform/providers",
        "/api/v1/platform/deployments",
        "/api/v1/platform/models",
    ] {
        for q in ["%0A", "%0Aabc%0A", "%09abc"] {
            let (status, _) =
                call(&f.s, &f.admin, "GET", &format!("{path}?q={q}"), Value::Null).await;
            if status != StatusCode::OK {
                wrong.push(format!("{path} trimmed q={q}: {status}"));
            }
        }
    }
    assert!(wrong.is_empty(), "edge inputs:\n{}", wrong.join("\n"));
}
