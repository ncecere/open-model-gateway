use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use axum::{
    extract::Request,
    http::{Method, StatusCode, header},
    response::{IntoResponse, Response},
};
use percent_encoding::percent_decode_str;
use tower::ServiceExt;
use tower_http::services::{ServeDir, ServeFile};

/// Only point this at a trusted frontend build directory, never the repository root.
#[derive(Clone)]
pub struct WebAssets {
    directory: PathBuf,
}

impl WebAssets {
    pub fn load(directory: &Path) -> Result<Self> {
        let directory = directory
            .canonicalize()
            .context("GATEWAY_WEB_DIR does not exist")?;
        ensure!(directory.is_dir(), "GATEWAY_WEB_DIR must be a directory");
        ensure!(
            directory.join("index.html").is_file(),
            "GATEWAY_WEB_DIR must contain index.html; run `npm run build:web` first"
        );
        Ok(Self { directory })
    }

    /// Readiness: the loaded build has not been removed or unmounted.
    pub fn is_intact(&self) -> bool {
        self.directory.join("index.html").is_file()
    }

    pub async fn serve(&self, request: Request) -> Response {
        let Ok(path) = percent_decode_str(request.uri().path()).decode_utf8() else {
            return super::http::not_found();
        };
        let segments: Vec<_> = path
            .split('/')
            .filter(|segment| !segment.is_empty())
            .collect();
        // Never let HTML fallback hide an API typo, including encoded prefixes.
        if matches!(segments.first(), Some(&"api" | &"v1" | &"health" | &"scim"))
            || segments
                .iter()
                .any(|segment| segment.starts_with('.') || segment.contains('\\'))
        {
            return super::http::not_found();
        }
        if !matches!(*request.method(), Method::GET | Method::HEAD) {
            return StatusCode::METHOD_NOT_ALLOWED.into_response();
        }

        let index = ServeFile::new(self.directory.join("index.html"));
        if path == "/" {
            return index.oneshot(request).await.unwrap().into_response();
        }
        let is_navigation = request
            .headers()
            .get(header::ACCEPT)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|accept| {
                accept
                    .split(',')
                    .any(|part| part.trim().split(';').next() == Some("text/html"))
            });
        let is_asset = segments.first() == Some(&"assets")
            || segments.iter().any(|segment| segment.contains('.'));
        let files = ServeDir::new(&self.directory).append_index_html_on_directories(false);
        if is_navigation && !is_asset {
            files
                .fallback(index)
                .oneshot(request)
                .await
                .unwrap()
                .into_response()
        } else {
            files.oneshot(request).await.unwrap().into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{http::router_with_web, store::Store};
    use axum::{
        Router,
        body::{Body, to_bytes},
    };
    use sqlx::postgres::PgPoolOptions;

    fn fixture() -> (tempfile::TempDir, Router) {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(
            directory.path().join("index.html"),
            "<!doctype html><title>Gateway</title>",
        )
        .unwrap();
        std::fs::write(directory.path().join(".env"), "SECRET=never-serve-this").unwrap();
        std::fs::create_dir(directory.path().join("assets")).unwrap();
        std::fs::write(
            directory.path().join("assets/app.js"),
            "console.log('gateway');",
        )
        .unwrap();
        let pool = PgPoolOptions::new()
            .connect_lazy("postgres://unused:unused@127.0.0.1:1/unused")
            .unwrap();
        let app = router_with_web(
            Store::new(pool),
            Some(WebAssets::load(directory.path()).unwrap()),
        );
        (directory, app)
    }

    fn request(uri: &str, method: Method, accept: &str) -> Request {
        Request::builder()
            .uri(uri)
            .method(method)
            .header(header::ACCEPT, accept)
            .body(Body::empty())
            .unwrap()
    }

    #[test]
    fn configured_web_directory_must_have_an_index() {
        let directory = tempfile::tempdir().unwrap();
        assert!(WebAssets::load(&directory.path().join("missing")).is_err());
        assert!(WebAssets::load(directory.path()).is_err());
        std::fs::write(directory.path().join("file"), "not a directory").unwrap();
        assert!(WebAssets::load(&directory.path().join("file")).is_err());
    }

    #[tokio::test]
    async fn serves_root_assets_and_browser_deep_links() {
        let (_directory, app) = fixture();
        for path in ["/", "/workspaces/example", "/workspaces/example/"] {
            let response = app
                .clone()
                .oneshot(request(path, Method::GET, "text/html"))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{path}");
            assert!(
                response.headers()[header::CONTENT_TYPE]
                    .to_str()
                    .unwrap()
                    .starts_with("text/html")
            );
            assert!(response.headers().contains_key("x-request-id"));
            assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        }
        let response = app
            .clone()
            .oneshot(request("/assets/app.js", Method::GET, "*/*"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(
            response.headers()[header::CONTENT_TYPE]
                .to_str()
                .unwrap()
                .contains("javascript")
        );
        let response = app
            .oneshot(request("/workspaces/example", Method::HEAD, "text/html"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(
            to_bytes(response.into_body(), 4096)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn never_serves_html_for_unknown_api_paths() {
        let (_directory, app) = fixture();
        for path in [
            "/api",
            "/api/v1/unknown",
            "/v1",
            "/v1/unknown",
            "/health",
            "/health/missing",
            "/scim",
            "/scim/v2/Users",
            "/%76%31/unknown",
            "//api/v1/unknown",
        ] {
            let response = app
                .clone()
                .oneshot(request(path, Method::GET, "text/html"))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
            assert_eq!(
                response.headers()[header::CONTENT_TYPE],
                "application/json",
                "{path}"
            );
        }
    }

    #[tokio::test]
    async fn missing_assets_traversal_and_non_navigation_do_not_get_spa_html() {
        let (_directory, app) = fixture();
        for path in [
            "/assets/missing.js",
            "/assets/missing",
            "/missing.css",
            "/favicon.ico",
            "/.env",
            "/%2eenv",
            "/%2e%2e/secret",
            "/assets/%2e%2e/index.html",
        ] {
            let response = app
                .clone()
                .oneshot(request(path, Method::GET, "text/html"))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
        }
        let response = app
            .clone()
            .oneshot(request(
                "/workspaces/example",
                Method::GET,
                "application/json",
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let response = app
            .oneshot(request("/workspaces/example", Method::POST, "text/html"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
    }

    #[tokio::test]
    async fn static_frontend_does_not_bypass_auth_or_health_handlers() {
        let (_directory, app) = fixture();
        let response = app
            .clone()
            .oneshot(request("/v1/models", Method::GET, "text/html"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let response = app
            .oneshot(request("/health/live", Method::GET, "text/html"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CONTENT_TYPE], "application/json");
    }
}
