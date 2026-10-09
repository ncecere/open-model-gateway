use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use axum::{
    Extension, Json, Router,
    extract::{DefaultBodyLimit, MatchedPath, Request, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{MethodRouter, get, post},
};
use serde_json::json;
use tower_http::limit::RequestBodyLimitLayer;
use tracing::Instrument;
use uuid::Uuid;

use crate::{
    auth::Principal,
    identity::IdentityState,
    inference::{Engine, EngineLimits, types::WorkloadKind},
    protocols::{
        audio, batches, chat_completions, embeddings, files, images, messages, realtime, rerank,
        responses, systemone, videos,
    },
    providers::ProviderRegistry,
    store::Store,
    web::WebAssets,
};

#[derive(Clone, Copy)]
pub struct RequestId(pub Uuid);

#[derive(Clone, Copy)]
enum Protocol {
    OpenAi,
    Anthropic,
}

pub fn router(store: Store) -> Router {
    router_with_web(store, None)
}

pub fn router_with_web(store: Store, web: Option<WebAssets>) -> Router {
    let engine = Engine::new(
        Arc::new(store.clone()),
        ProviderRegistry::default(),
        EngineLimits::default(),
    )
    .expect("valid default engine limits");
    router_with_engine(store, web, engine)
}

pub fn router_with_engine(store: Store, web: Option<WebAssets>, engine: Engine) -> Router {
    build_router(store, web, engine, None)
}

pub fn router_with_identity(
    store: Store,
    web: Option<WebAssets>,
    engine: Engine,
    identity: IdentityState,
) -> Router {
    build_router(store, web, engine, Some(identity))
}

fn build_router(
    store: Store,
    web: Option<WebAssets>,
    engine: Engine,
    identity: Option<IdentityState>,
) -> Router {
    let inference = Router::new()
        .route("/v1/models", get(models))
        .route("/v1/chat/completions", post(chat_completions::handle))
        .route("/v1/responses", post(responses::handle))
        .route("/v1/messages", post(messages::handle))
        .route("/v1/embeddings", post(embeddings::handle))
        // Explicitly unsupported realtime companions (never minted upstream).
        .route(
            "/v1/realtime/client_secrets",
            post(realtime::unsupported_route),
        )
        .route("/v1/realtime/calls", post(realtime::unsupported_route))
        .route_layer(middleware::from_fn(client_labels))
        .route_layer(middleware::from_fn_with_state(store.clone(), authenticate));
    // Non-generation workloads carry their own configured body caps instead
    // of the shared 2 MiB limit (transcriptions also cap the file part).
    let limits = engine.limits().workloads;
    let capped = |kind: WorkloadKind, route: MethodRouter<Store>| -> MethodRouter<Store> {
        let bytes = limits.body_bytes(kind).expect("workload body cap");
        route
            .layer::<_, std::convert::Infallible>(DefaultBodyLimit::max(bytes))
            .layer(RequestBodyLimitLayer::new(bytes))
    };
    let workloads = Router::new()
        .route(
            "/v1/images/generations",
            capped(WorkloadKind::Images, post(images::handle)),
        )
        .route(
            "/v1/rerank",
            capped(WorkloadKind::Rerank, post(rerank::handle)),
        )
        .route(
            "/v1/systemone",
            capped(WorkloadKind::Systemone, post(systemone::handle)),
        )
        .route(
            "/v1/audio/transcriptions",
            capped(
                WorkloadKind::AudioTranscriptions,
                post(audio::transcriptions),
            ),
        )
        .route(
            "/v1/audio/speech",
            capped(WorkloadKind::AudioSpeech, post(audio::speech)),
        )
        .route_layer(middleware::from_fn(client_labels))
        .route_layer(middleware::from_fn_with_state(store.clone(), authenticate));
    // Async jobs (`crate::jobs`): video create and the streamed batch-file
    // upload have their own caps; everything else keeps 2 MiB.
    let job_limits = crate::jobs::limits();
    let cap = |bytes: usize, route: MethodRouter<Store>| -> MethodRouter<Store> {
        route
            .layer::<_, std::convert::Infallible>(DefaultBodyLimit::max(bytes))
            .layer(RequestBodyLimitLayer::new(bytes))
    };
    let file_bytes = crate::filestore::upload::limits().body_bytes();
    let small = 2 * 1024 * 1024;
    let jobs = Router::new()
        .route(
            "/v1/videos",
            cap(
                job_limits.video_body_bytes,
                post(videos::create).get(videos::list),
            ),
        )
        .route(
            "/v1/videos/{id}",
            cap(small, get(videos::retrieve).delete(videos::delete)),
        )
        .route("/v1/videos/{id}/content", cap(small, get(videos::content)))
        // Gateway-owned Files API on the encrypted file store (docs/files-api.md).
        .route(
            "/v1/files",
            cap(file_bytes, post(files::upload).get(files::list)),
        )
        .route(
            "/v1/files/{id}",
            cap(small, get(files::retrieve).delete(files::delete)),
        )
        .route("/v1/files/{id}/content", cap(small, get(files::content)))
        .route(
            "/v1/batches",
            cap(small, post(batches::create).get(batches::list)),
        )
        .route("/v1/batches/{id}", cap(small, get(batches::retrieve)))
        .route("/v1/batches/{id}/cancel", cap(small, post(batches::cancel)))
        .route_layer(middleware::from_fn(client_labels))
        .route_layer(middleware::from_fn_with_state(store.clone(), authenticate));

    let ready_web = web.clone();
    let mut root = Router::new();
    // Dashboard file uploads carry their own cap (GATEWAY_FILES_MAX_BYTES).
    let mut uploads = Router::new();
    if let Some(identity) = identity {
        uploads = crate::management::upload_router(identity.clone());
        root = root
            .merge(crate::identity::router(identity.clone()))
            .merge(crate::management::router(identity));
    }
    root.route(
        "/health/live",
        get(|| async { Json(json!({"status": "ok"})) }),
    )
    .route(
        "/health/ready",
        get(move |State(store): State<Store>| readiness(store, ready_web.clone())),
    )
    .merge(inference)
    // Public installation logo (no session; same-origin for the sign-in page).
    .merge(crate::management::public_router())
    // WebSocket upgrade: authenticates itself (Bearer header or the key
    // subprotocol) before admission; no request body.
    .route("/v1/realtime", get(realtime::handle))
    .fallback(move |request: Request| {
        let web = web.clone();
        async move {
            match web {
                Some(web) => web.serve(request).await,
                None => not_found(),
            }
        }
    })
    .layer(RequestBodyLimitLayer::new(2 * 1024 * 1024))
    .merge(workloads)
    .merge(jobs)
    .merge(uploads)
    .layer(Extension(engine))
    .layer(middleware::from_fn(request_context))
    .with_state(store)
}

/// Response extension: keep the handler's own `Cache-Control` (public,
/// non-personal content only, such as the installation logo).
#[derive(Clone, Copy, Debug)]
pub(crate) struct PublicCache;

async fn request_context(mut request: Request, next: Next) -> Response {
    // Generate our own ID; do not reflect arbitrary client-supplied values into logs.
    let id = Uuid::new_v4();
    request.extensions_mut().insert(RequestId(id));
    let request_id = id.to_string();
    let span = tracing::info_span!("http.request", request_id, method = %request.method());
    // Route templates only (never raw paths); unrouted/SPA requests share one label.
    let route = request
        .extensions()
        .get::<MatchedPath>()
        .map_or("unmatched", |path| path.as_str())
        .to_owned();
    let method = request.method().clone();
    async {
        let started = Instant::now();
        let mut response = next.run(request).await;
        crate::metrics::METRICS.observe_http(
            &method,
            &route,
            response.status().as_u16(),
            started.elapsed(),
        );
        response.headers_mut().insert(
            "x-request-id",
            HeaderValue::from_str(&request_id).expect("UUID is a valid header value"),
        );
        // Everything is uncacheable unless a handler marked it public (the logo).
        if response.extensions().get::<PublicCache>().is_none() {
            response
                .headers_mut()
                .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        }
        tracing::info!(
            status = response.status().as_u16(),
            header_latency_ms = started.elapsed().as_millis(),
            "response headers ready"
        );
        response
    }
    .instrument(span)
    .await
}

fn credential(headers: &HeaderMap, protocol: Protocol) -> Option<&str> {
    let authorization_count = headers.get_all(header::AUTHORIZATION).iter().count();
    let api_key_count = headers.get_all("x-api-key").iter().count();
    if authorization_count > 1
        || api_key_count > 1
        || (authorization_count > 0 && api_key_count > 0)
    {
        return None;
    }
    if let Some(value) = headers.get(header::AUTHORIZATION) {
        let (scheme, token) = value.to_str().ok()?.split_once(' ')?;
        if !scheme.eq_ignore_ascii_case("bearer")
            || token.is_empty()
            || token.bytes().any(|b| b.is_ascii_whitespace())
        {
            return None;
        }
        return Some(token);
    }
    if matches!(protocol, Protocol::Anthropic) {
        return headers.get("x-api-key")?.to_str().ok();
    }
    None
}

/// Optional client labels (`X-Session-Id`, `X-Title`) for Logs, visible to
/// admission through a task-local scope. Never authorization input.
async fn client_labels(request: Request, next: Next) -> Response {
    crate::inference::client::ClientMetadata::from_headers(request.headers())
        .scope(next.run(request))
        .await
}

async fn authenticate(State(store): State<Store>, mut request: Request, next: Next) -> Response {
    let protocol = if request.uri().path() == "/v1/messages" {
        Protocol::Anthropic
    } else {
        Protocol::OpenAi
    };
    let Some(token) = credential(request.headers(), protocol) else {
        return unauthorized(protocol);
    };
    match store.authenticate(token).await {
        Ok(Some(principal)) => {
            request.extensions_mut().insert(principal);
            next.run(request).await
        }
        Ok(None) => unauthorized(protocol),
        Err(_) => {
            tracing::error!("authentication database lookup failed");
            api_error(
                protocol,
                StatusCode::SERVICE_UNAVAILABLE,
                "api_error",
                "Authentication service unavailable",
            )
        }
    }
}

fn unauthorized(protocol: Protocol) -> Response {
    let mut response = api_error(
        protocol,
        StatusCode::UNAUTHORIZED,
        "authentication_error",
        "Invalid or missing API key",
    );
    response
        .headers_mut()
        .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
    response
}

async fn models(
    State(store): State<Store>,
    Extension(principal): Extension<Principal>,
) -> Response {
    match store.visible_models(&principal).await {
        Ok(models) => Json(json!({"object": "list", "data": models})).into_response(),
        Err(_) => {
            tracing::error!("model catalog database lookup failed");
            api_error(
                Protocol::OpenAi,
                StatusCode::SERVICE_UNAVAILABLE,
                "api_error",
                "Model catalog unavailable",
            )
        }
    }
}

/// Ready only when the database answers, the schema lineage exactly matches
/// this binary, and (if configured) the served web build is still intact.
/// Check names and ok/fail only; never error details.
async fn readiness(store: Store, web: Option<WebAssets>) -> Response {
    let checks = tokio::time::timeout(Duration::from_secs(2), store.readiness())
        .await
        .unwrap_or_default();
    let web_ok = web.as_ref().is_none_or(WebAssets::is_intact);
    let ready = checks.database && checks.schema && web_ok;
    let word = |ok: bool| if ok { "ok" } else { "fail" };
    let body = json!({
        "status": if ready { "ready" } else { "not_ready" },
        "checks": {
            "database": word(checks.database),
            "schema": if checks.database { word(checks.schema) } else { "unknown" },
            "web": if web.is_some() { word(web_ok) } else { "disabled" },
        },
    });
    let status = if ready {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (status, Json(body)).into_response()
}

pub(crate) fn not_found() -> Response {
    api_error(
        Protocol::OpenAi,
        StatusCode::NOT_FOUND,
        "not_found_error",
        "Route not found",
    )
}

fn api_error(protocol: Protocol, status: StatusCode, kind: &str, message: &str) -> Response {
    let body = match protocol {
        Protocol::OpenAi => {
            json!({"error": {"message": message, "type": kind, "param": null, "code": kind}})
        }
        Protocol::Anthropic => {
            json!({"type": "error", "error": {"type": kind, "message": message}})
        }
    };
    (status, Json(body)).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{Body, to_bytes};
    use sqlx::postgres::PgPoolOptions;
    use tower::ServiceExt;

    fn app() -> Router {
        let pool = PgPoolOptions::new()
            .connect_lazy("postgres://unused:unused@127.0.0.1:1/unused")
            .unwrap();
        router(Store::new(pool))
    }

    #[tokio::test]
    async fn liveness_does_not_need_database_and_overrides_request_id() {
        let response = app()
            .oneshot(
                Request::builder()
                    .uri("/health/live")
                    .header("x-request-id", "untrusted")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(Uuid::parse_str(response.headers()["x-request-id"].to_str().unwrap()).is_ok());
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    }

    #[tokio::test]
    async fn all_inference_routes_require_authentication() {
        for (path, method) in [
            ("/v1/models", "GET"),
            ("/v1/chat/completions", "POST"),
            ("/v1/responses", "POST"),
            ("/v1/messages", "POST"),
            ("/v1/embeddings", "POST"),
            ("/v1/rerank", "POST"),
            ("/v1/systemone", "POST"),
            ("/v1/images/generations", "POST"),
            ("/v1/audio/transcriptions", "POST"),
            ("/v1/audio/speech", "POST"),
            // Async jobs: every route, including reads, is key-authenticated.
            ("/v1/videos", "POST"),
            ("/v1/videos", "GET"),
            ("/v1/videos/video_00000000000000000000000000000000", "GET"),
            (
                "/v1/videos/video_00000000000000000000000000000000",
                "DELETE",
            ),
            (
                "/v1/videos/video_00000000000000000000000000000000/content",
                "GET",
            ),
            ("/v1/files", "POST"),
            ("/v1/files", "GET"),
            ("/v1/files/file-00000000000000000000000000000000", "GET"),
            ("/v1/files/file-00000000000000000000000000000000", "DELETE"),
            (
                "/v1/files/file-00000000000000000000000000000000/content",
                "GET",
            ),
            ("/v1/batches", "POST"),
            ("/v1/batches", "GET"),
            ("/v1/batches/batch_00000000000000000000000000000000", "GET"),
            (
                "/v1/batches/batch_00000000000000000000000000000000/cancel",
                "POST",
            ),
        ] {
            let response = app()
                .oneshot(
                    Request::builder()
                        .uri(path)
                        .method(method)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{path}");
            let body: serde_json::Value =
                serde_json::from_slice(&to_bytes(response.into_body(), 4096).await.unwrap())
                    .unwrap();
            assert_eq!(body["error"]["type"], "authentication_error");
            if path == "/v1/messages" {
                assert_eq!(body["type"], "error");
            }
        }
    }

    #[tokio::test]
    async fn readiness_is_structured_and_fails_closed_without_database() {
        let pool = PgPoolOptions::new()
            .acquire_timeout(Duration::from_millis(200))
            .connect_lazy("postgres://unused:unused@127.0.0.1:1/unused")
            .unwrap();
        let response = router(Store::new(pool))
            .oneshot(
                Request::builder()
                    .uri("/health/ready")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body: serde_json::Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 4096).await.unwrap()).unwrap();
        assert_eq!(
            body,
            json!({"status":"not_ready","checks":{"database":"fail","schema":"unknown","web":"disabled"}})
        );
    }

    #[tokio::test]
    async fn requests_are_counted_by_route_template_not_raw_path() {
        let before = crate::metrics::METRICS.render(None).await;
        assert!(!before.contains("/not-a-route-for-metrics"));
        app()
            .oneshot(
                Request::builder()
                    .uri("/not-a-route-for-metrics")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        app()
            .oneshot(
                Request::builder()
                    .uri("/health/live")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let after = crate::metrics::METRICS.render(None).await;
        assert!(!after.contains("/not-a-route-for-metrics"));
        assert!(after.contains(r#"route="unmatched",status="404""#));
        assert!(after.contains(r#"route="/health/live",status="200""#));
    }

    #[tokio::test]
    async fn unknown_route_is_not_a_fake_success() {
        let response = app()
            .oneshot(
                Request::builder()
                    .uri("/not-a-route")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn audio_routes_reject_bad_credentials_before_parsing() {
        // Every workload kind now has a served route; audio is key-authenticated
        // before any multipart/JSON parsing.
        for path in ["/v1/audio/transcriptions", "/v1/audio/speech"] {
            let response = app()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri(path)
                        .header(header::AUTHORIZATION, "Bearer synthetic")
                        .header(header::CONTENT_TYPE, "multipart/form-data")
                        .body(Body::from("not a form"))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{path}");
        }
    }

    #[test]
    fn header_schemes_are_strict_and_ambiguous_credentials_are_rejected() {
        let mut headers = HeaderMap::new();
        headers.insert("x-api-key", HeaderValue::from_static("abc"));
        assert_eq!(credential(&headers, Protocol::Anthropic), Some("abc"));
        assert_eq!(credential(&headers, Protocol::OpenAi), None);
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer def"),
        );
        assert_eq!(credential(&headers, Protocol::Anthropic), None);
        headers.remove("x-api-key");
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("bEaReR def"),
        );
        assert_eq!(credential(&headers, Protocol::OpenAi), Some("def"));
        headers.append(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer other"),
        );
        assert_eq!(credential(&headers, Protocol::OpenAi), None);
    }
}
