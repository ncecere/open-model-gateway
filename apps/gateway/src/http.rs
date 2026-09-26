use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use axum::{
    Extension, Json, Router,
    extract::{Request, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde_json::json;
use tower_http::limit::RequestBodyLimitLayer;
use tracing::Instrument;
use uuid::Uuid;

use crate::{
    auth::Principal,
    identity::IdentityState,
    inference::{Engine, EngineLimits},
    protocols::{chat_completions, messages, responses},
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
        .route_layer(middleware::from_fn_with_state(store.clone(), authenticate));

    let mut root = Router::new();
    if let Some(identity) = identity {
        root = root
            .merge(crate::identity::router(identity.clone()))
            .merge(crate::management::router(identity));
    }
    root.route(
        "/health/live",
        get(|| async { Json(json!({"status": "ok"})) }),
    )
    .route("/health/ready", get(readiness))
    .merge(inference)
    .fallback(move |request: Request| {
        let web = web.clone();
        async move {
            match web {
                Some(web) => web.serve(request).await,
                None => not_found(),
            }
        }
    })
    .layer(Extension(engine))
    .layer(RequestBodyLimitLayer::new(2 * 1024 * 1024))
    .layer(middleware::from_fn(request_context))
    .with_state(store)
}

async fn request_context(mut request: Request, next: Next) -> Response {
    // Generate our own ID; do not reflect arbitrary client-supplied values into logs.
    let id = Uuid::new_v4();
    request.extensions_mut().insert(RequestId(id));
    let request_id = id.to_string();
    let span = tracing::info_span!("http.request", request_id, method = %request.method());
    async {
        let started = Instant::now();
        let mut response = next.run(request).await;
        response.headers_mut().insert(
            "x-request-id",
            HeaderValue::from_str(&request_id).expect("UUID is a valid header value"),
        );
        response
            .headers_mut()
            .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
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

async fn readiness(State(store): State<Store>) -> Response {
    if tokio::time::timeout(Duration::from_secs(2), store.is_ready())
        .await
        .unwrap_or(false)
    {
        (StatusCode::OK, Json(json!({"status": "ready"}))).into_response()
    } else {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"status": "not_ready"})),
        )
            .into_response()
    }
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
