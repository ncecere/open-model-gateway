//! Installation logo (0023): authority, the store-off gate, upload validation
//! (types, SVG and polyglots, size, dimensions), the public endpoint's headers,
//! replace/remove deleting the previous object, auditing and `/me`.
use super::*;
use crate::filestore::{FileStoreRuntime, ObjectKey};
use branding::fixtures::{jpeg, png, webp_lossless};

const LOGO: &str = "/api/v1/platform/settings/general/logo";
const GENERAL: &str = "/api/v1/platform/settings/general";
const PUBLIC: &str = "/api/v1/branding/logo";

async fn send(
    s: &Store,
    u: &BrowserPrincipal,
    rt: Option<&FileStoreRuntime>,
    method: &str,
    path: &str,
    content_type: &str,
    body: Vec<u8>,
) -> (StatusCode, Value) {
    let mut router = routes().layer(Extension(u.clone()));
    if let Some(rt) = rt {
        router = router.layer(Extension(rt.clone()));
    }
    let response = router
        .with_state(s.clone())
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .header("content-type", content_type)
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// The public endpoint through the full router (request middleware included).
async fn public(
    s: &Store,
    rt: Option<&FileStoreRuntime>,
    etag: Option<&str>,
) -> axum::http::Response<Body> {
    let mut router = crate::http::router(s.clone());
    if let Some(rt) = rt {
        router = router.layer(Extension(rt.clone()));
    }
    let mut request = Request::builder().uri(format!("{PUBLIC}?v=abc"));
    if let Some(etag) = etag {
        request = request.header("if-none-match", etag);
    }
    router
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap()
}

async fn audit_count(pool: &PgPool, action: &str) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM audit_events WHERE action=$1")
        .bind(action)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn object_exists(pool: &PgPool, rt: &FileStoreRuntime, id: Uuid) -> bool {
    let key: String = sqlx::query_scalar("SELECT object_key FROM stored_files WHERE id=$1")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap();
    rt.store()
        .unwrap()
        .head(&ObjectKey::parse(&key).unwrap())
        .await
        .unwrap()
        .is_some()
}

async fn current(pool: &PgPool) -> Option<Uuid> {
    sqlx::query_scalar("SELECT branding_logo_file_id FROM installation_settings WHERE singleton")
        .fetch_one(pool)
        .await
        .unwrap()
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn logo_is_admin_write_auditor_read(pool: PgPool) {
    let f = fixture(&pool).await;
    let rt = FileStoreRuntime::memory();
    for user in [&f.auditor, &f.owner, &f.member] {
        for method in ["PUT", "DELETE"] {
            let (status, _) = send(
                &f.s,
                user,
                Some(&rt),
                method,
                LOGO,
                "image/png",
                png(64, 64),
            )
            .await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{method}");
        }
    }
    let (status, v) = send(
        &f.s,
        &f.admin,
        Some(&rt),
        "PUT",
        LOGO,
        "image/png",
        png(64, 64),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{v}");
    assert_eq!(v["logo"]["width"], 64);
    assert!(
        v["logo"]["url"]
            .as_str()
            .unwrap()
            .starts_with("/api/v1/branding/logo?v=")
    );
    // Auditors read the logo and upload limits; Users don't see settings.
    let (status, v) = send(
        &f.s,
        &f.auditor,
        Some(&rt),
        "GET",
        GENERAL,
        "application/json",
        vec![],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["logo"]["height"], 64);
    assert_eq!(v["logo_upload"]["available"], true);
    assert_eq!(v["logo_upload"]["max_bytes"], 512 * 1024);
    for user in [&f.owner, &f.member] {
        let (status, _) = send(
            &f.s,
            user,
            Some(&rt),
            "GET",
            GENERAL,
            "application/json",
            vec![],
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }
    // Every signed-in user sees it in /me; the public endpoint needs no session.
    let (status, me) = send(
        &f.s,
        &f.member,
        Some(&rt),
        "GET",
        "/api/v1/me",
        "application/json",
        vec![],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(me["installation"]["logo"]["url"], v["logo"]["url"]);
    assert!(me["installation"]["logo"]["updated_at"].is_string());
    assert_eq!(public(&f.s, Some(&rt), None).await.status(), StatusCode::OK);
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn logo_requires_the_file_store(pool: PgPool) {
    let f = fixture(&pool).await;
    let (status, v) = send(&f.s, &f.admin, None, "PUT", LOGO, "image/png", png(64, 64)).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(v["error"]["reason"], "file_storage_not_configured");
    let (_, v) = send(
        &f.s,
        &f.admin,
        None,
        "GET",
        GENERAL,
        "application/json",
        vec![],
    )
    .await;
    assert_eq!(v["logo"], Value::Null);
    assert_eq!(v["logo_upload"]["available"], false);
    let response = public(&f.s, None, None).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(response.headers()["cache-control"], "no-store");
    let (_, me) = send(
        &f.s,
        &f.member,
        None,
        "GET",
        "/api/v1/me",
        "application/json",
        vec![],
    )
    .await;
    assert_eq!(me["installation"]["logo"], Value::Null);
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn logo_upload_validates_type_structure_size_and_dimensions(pool: PgPool) {
    let f = fixture(&pool).await;
    let rt = FileStoreRuntime::memory();
    let svg =
        br#"<svg xmlns="http://www.w3.org/2000/svg"><script>alert(1)</script></svg>"#.to_vec();
    let mut polyglot = png(64, 64);
    polyglot.extend_from_slice(b"<html><script>alert(1)</script></html>");
    let mut big = png(64, 64);
    big.resize(512 * 1024 + 1, 0);
    for (declared, body, status, reason) in [
        (
            "image/svg+xml",
            svg.clone(),
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "logo_unsupported_type",
        ),
        (
            "image/png",
            svg,
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "logo_unsupported_type",
        ),
        (
            "image/webp",
            png(64, 64),
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "logo_unsupported_type",
        ),
        (
            "application/octet-stream",
            png(64, 64),
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "logo_unsupported_type",
        ),
        (
            "image/png",
            polyglot,
            StatusCode::BAD_REQUEST,
            "logo_invalid_image",
        ),
        (
            "image/png",
            big,
            StatusCode::PAYLOAD_TOO_LARGE,
            "logo_too_large",
        ),
        (
            "image/png",
            png(8, 8),
            StatusCode::BAD_REQUEST,
            "logo_dimensions",
        ),
    ] {
        let (s, v) = send(&f.s, &f.admin, Some(&rt), "PUT", LOGO, declared, body).await;
        assert_eq!(s, status, "{declared} {v}");
        assert_eq!(v["error"]["reason"], reason, "{declared}");
    }
    let stored: i64 = sqlx::query_scalar("SELECT count(*) FROM stored_files")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(stored, 0, "rejected uploads never reach the store");
    for (declared, body) in [
        ("image/jpeg", jpeg(80, 80)),
        ("image/webp", webp_lossless(64, 32)),
    ] {
        let (s, v) = send(&f.s, &f.admin, Some(&rt), "PUT", LOGO, declared, body).await;
        assert_eq!(s, StatusCode::OK, "{declared} {v}");
    }
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn public_logo_is_served_with_safe_cacheable_headers(pool: PgPool) {
    let f = fixture(&pool).await;
    let rt = FileStoreRuntime::memory();
    assert_eq!(
        public(&f.s, Some(&rt), None).await.status(),
        StatusCode::NOT_FOUND
    );
    let image = webp_lossless(96, 96);
    let (status, _) = send(
        &f.s,
        &f.admin,
        Some(&rt),
        "PUT",
        LOGO,
        "image/webp",
        image.clone(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let response = public(&f.s, Some(&rt), None).await;
    assert_eq!(response.status(), StatusCode::OK);
    let h = response.headers().clone();
    assert_eq!(h["content-type"], "image/webp");
    assert_eq!(h["x-content-type-options"], "nosniff");
    assert_eq!(h["content-security-policy"], "default-src 'none'");
    assert_eq!(h["cache-control"], "public, max-age=300");
    assert_eq!(h["cross-origin-resource-policy"], "same-origin");
    let etag = h["etag"].to_str().unwrap().to_owned();
    assert!(etag.starts_with('"') && etag.len() == 66);
    let body = to_bytes(response.into_body(), 1 << 20).await.unwrap();
    assert_eq!(body.as_ref(), image.as_slice());
    let response = public(&f.s, Some(&rt), Some(&etag)).await;
    assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
    assert_eq!(response.headers()["etag"], etag.as_str());
    assert!(
        to_bytes(response.into_body(), 1024)
            .await
            .unwrap()
            .is_empty()
    );
    // The public sign-in configuration exposes it, with the name as alt text.
    let config = sign_in_config(&f.s, &rt).await;
    assert!(config["logo"]["url"].as_str().unwrap().starts_with(PUBLIC));
    assert!(config["logo"]["updated_at"].is_string());
    assert!(config["installation_name"].is_string());
    let (status, _) = send(
        &f.s,
        &f.admin,
        Some(&rt),
        "DELETE",
        LOGO,
        "application/json",
        vec![],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let config = sign_in_config(&f.s, &rt).await;
    assert_eq!(config["logo"], Value::Null);
    assert_eq!(config["installation_name"], Value::Null);
}

async fn sign_in_config(s: &Store, rt: &FileStoreRuntime) -> Value {
    let identity = crate::identity::IdentityState::new(s.clone(), None)
        .await
        .unwrap();
    let response = crate::identity::router(identity)
        .layer(Extension(rt.clone()))
        .with_state(s.clone())
        .oneshot(
            Request::builder()
                .uri("/api/v1/auth/config")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["cache-control"], "no-store");
    serde_json::from_slice(&to_bytes(response.into_body(), 1 << 16).await.unwrap()).unwrap()
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn replacing_and_removing_delete_the_previous_logo(pool: PgPool) {
    let f = fixture(&pool).await;
    let rt = FileStoreRuntime::memory();
    // A deprecated external URL is replaced by the first upload.
    sqlx::query("UPDATE installation_settings SET logo_url='https://cdn.example.test/logo.png' WHERE singleton")
        .execute(&pool)
        .await
        .unwrap();
    let (_, first) = send(
        &f.s,
        &f.admin,
        Some(&rt),
        "PUT",
        LOGO,
        "image/png",
        png(64, 64),
    )
    .await;
    assert_eq!(first["logo_url"], Value::Null);
    let one = current(&pool).await.unwrap();
    assert!(object_exists(&pool, &rt, one).await);
    let (_, second) = send(
        &f.s,
        &f.admin,
        Some(&rt),
        "PUT",
        LOGO,
        "image/jpeg",
        jpeg(128, 64),
    )
    .await;
    assert_ne!(first["logo"]["url"], second["logo"]["url"]);
    assert_eq!(second["logo"]["width"], 128);
    let two = current(&pool).await.unwrap();
    assert_ne!(one, two);
    assert!(!object_exists(&pool, &rt, one).await);
    let deleted: bool =
        sqlx::query_scalar("SELECT deleted_at IS NOT NULL FROM stored_files WHERE id=$1")
            .bind(one)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(deleted);
    let (status, v) = send(
        &f.s,
        &f.admin,
        Some(&rt),
        "DELETE",
        LOGO,
        "application/json",
        vec![],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["logo"], Value::Null);
    assert_eq!(current(&pool).await, None);
    assert!(!object_exists(&pool, &rt, two).await);
    assert_eq!(
        public(&f.s, Some(&rt), None).await.status(),
        StatusCode::NOT_FOUND
    );
    // Removing again is a no-op (not audited twice).
    let (status, _) = send(
        &f.s,
        &f.admin,
        Some(&rt),
        "DELETE",
        LOGO,
        "application/json",
        vec![],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(audit_count(&pool, "settings.logo_uploaded").await, 2);
    assert_eq!(audit_count(&pool, "settings.logo_removed").await, 1);
    let live: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM stored_files WHERE purpose='branding' AND deleted_at IS NULL",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(live, 0);
}
