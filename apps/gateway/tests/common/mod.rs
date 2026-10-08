#![allow(dead_code)]

use axum::{
    body::{Body, to_bytes},
    http::{Method, Request, StatusCode},
};
use open_model_gateway::{
    http,
    identity::IdentityState,
    inference::{Engine, EngineLimits},
    providers::ProviderRegistry,
    store::Store,
};
use serde_json::Value;
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use std::sync::Arc;
use tower::ServiceExt;
use uuid::Uuid;

/// A real persisted session, verified by production require_session middleware.
/// This fixture does not claim to exercise OIDC sign-in or signature verification.
pub struct BrowserSession {
    pub user_id: Uuid,
    token: String,
    csrf: String,
}
impl BrowserSession {
    pub async fn new(pool: &PgPool, user_id: Uuid) -> Self {
        let token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
        let csrf = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
        // The fixture user's known email simulates a verified claim; it is not OIDC proof.
        let verified_email: String = sqlx::query_scalar("SELECT email FROM users WHERE id=$1")
            .bind(user_id)
            .fetch_one(pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO browser_sessions(token_hash,user_id,csrf_hash,expires_at,verified_email) VALUES($1,$2,$3,now()+interval '1 hour',$4)")
            .bind(Sha256::digest(token.as_bytes()).to_vec()).bind(user_id)
            .bind(Sha256::digest(csrf.as_bytes()).to_vec()).bind(verified_email).execute(pool).await.unwrap();
        Self {
            user_id,
            token,
            csrf,
        }
    }
    pub fn request(&self, method: Method, path: &str, body: Option<Value>) -> Request<Body> {
        Request::builder()
            .method(method)
            .uri(path)
            .header(
                "cookie",
                format!("omg_session={}; omg_csrf={}", self.token, self.csrf),
            )
            .header("content-type", "application/json")
            .body(body.map_or_else(Body::empty, |v| Body::from(v.to_string())))
            .unwrap()
    }
    pub async fn get(&self, app: &axum::Router, path: &str) -> (StatusCode, Value) {
        json_response(
            app.clone()
                .oneshot(self.request(Method::GET, path, None))
                .await
                .unwrap(),
        )
        .await
    }
}

pub async fn management_app(store: Store) -> axum::Router {
    let identity = IdentityState::new(store.clone(), None).await.unwrap();
    let engine = Engine::new(
        Arc::new(store.clone()),
        ProviderRegistry::default(),
        EngineLimits::default(),
    )
    .unwrap();
    http::router_with_identity(store, None, engine, identity)
}

pub async fn json_response(response: axum::response::Response) -> (StatusCode, Value) {
    let status = response.status();
    assert_eq!(response.headers()["cache-control"], "no-store");
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    let value = serde_json::from_slice(&bytes).unwrap_or_else(|_| {
        panic!(
            "non-JSON response ({status}): {}",
            String::from_utf8_lossy(&bytes)
        )
    });
    (status, value)
}

pub async fn user(pool: &PgPool, role: &str) -> Uuid {
    let user = Uuid::new_v4();
    sqlx::query("INSERT INTO users(id,email) VALUES($1,$2)")
        .bind(user)
        .bind(format!("{user}@fixture.invalid"))
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO platform_role_grants(user_id,role,source) VALUES($1,$2,'manual')")
        .bind(user)
        .bind(role)
        .execute(pool)
        .await
        .unwrap();
    user
}

pub async fn personal_workspace(pool: &PgPool, owner: Uuid) -> Uuid {
    let ws = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO workspaces(id,name,kind,owner_user_id) VALUES($1,'Private','personal',$2)",
    )
    .bind(ws)
    .bind(owner)
    .execute(pool)
    .await
    .unwrap();
    ws
}
