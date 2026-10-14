//! #144: the OIDC client (discovery, token exchange, JWKS) ignores ambient
//! proxy variables, like every provider client. This test binary sets those
//! variables, which is only safe because it runs in its own process with a
//! single test.
use axum::{Json, Router, routing::get};
use open_model_gateway::{
    identity::{IdentityConfig, IdentityState},
    store::Store,
};
use serde_json::json;
use sqlx::postgres::PgPoolOptions;

#[tokio::test]
async fn oidc_discovery_ignores_ambient_proxy_variables() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let issuer = format!("http://{}", listener.local_addr().unwrap());
    let metadata = json!({
        "issuer": issuer,
        "authorization_endpoint": format!("{issuer}/authorize"),
        "token_endpoint": format!("{issuer}/token"),
        "jwks_uri": format!("{issuer}/jwks"),
        "response_types_supported": ["code"],
        "subject_types_supported": ["public"],
        "id_token_signing_alg_values_supported": ["EdDSA"],
        "code_challenge_methods_supported": ["S256"]
    });
    // Discovery also fetches the key set.
    let app = Router::new()
        .route(
            "/.well-known/openid-configuration",
            get(move || {
                let metadata = metadata.clone();
                async move { Json(metadata) }
            }),
        )
        // RFC 8037's example Ed25519 public key.
        .route(
            "/jwks",
            get(|| async {
                Json(json!({"keys": [{"kty": "OKP", "crv": "Ed25519", "alg": "EdDSA", "use": "sig", "kid": "rfc8037", "x": "11qYAYKxCrfVS_7TyWQHOg7hcvPapiMlrwIaaPcHURo"}]}))
            }),
        );
    tokio::spawn(async move { axum::serve(listener, app).await });
    // A proxy that refuses every connection: honouring it fails discovery.
    let dead = "http://127.0.0.1:9";
    // SAFETY: the only test in this process; nothing else reads the environment concurrently.
    unsafe {
        for name in [
            "HTTP_PROXY",
            "http_proxy",
            "HTTPS_PROXY",
            "https_proxy",
            "ALL_PROXY",
            "all_proxy",
        ] {
            std::env::set_var(name, dead);
        }
        for name in ["NO_PROXY", "no_proxy"] {
            std::env::remove_var(name);
        }
        std::env::set_var("GATEWAY_ENV", "development");
        std::env::set_var("GATEWAY_PUBLIC_URL", "http://127.0.0.1:3000");
        std::env::set_var("GATEWAY_OIDC_ISSUER", &issuer);
        std::env::set_var("GATEWAY_OIDC_CLIENT_ID", "gateway");
    }
    let config = IdentityConfig::from_env().unwrap();
    let pool = PgPoolOptions::new()
        .connect_lazy("postgres://unused:unused@127.0.0.1:1/unused")
        .unwrap();
    IdentityState::new(Store::new(pool), config)
        .await
        .expect("OIDC discovery must not go through an ambient proxy");
}
