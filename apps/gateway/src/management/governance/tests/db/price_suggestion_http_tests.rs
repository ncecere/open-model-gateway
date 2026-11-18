//! Real loopback HTTP + disposable PostgreSQL proof of the operator handler.
//! Identity is injected as a browser principal; OIDC/CSRF are covered elsewhere.
use super::*;
use crate::management::governance::suggestion::{OpenRouterCatalog, tests::mock};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn operator_preview_is_bounded_fail_closed_and_preserves_prices(pool: PgPool) {
    let f = fixture(pool).await;
    let calls = Arc::new(AtomicUsize::new(0));
    let c = calls.clone();
    let catalog = axum::Router::new()
        .route("/models", axum::routing::get(move |headers: axum::http::HeaderMap| {
            assert!(headers.get("authorization").is_none());
            let c = c.clone();
            async move {
                // A corrected upstream response must be fetched immediately.
                if c.fetch_add(1, Ordering::SeqCst) == 0 {
                    return ([("content-type", "application/json")], "{broken".to_owned());
                }
                let tiers: Vec<_> = (1..=20_000).map(|n|json!({"min_prompt_tokens":n,"prompt":"1"})).collect();
                ([("content-type", "application/json")], json!({"data":[
                    {"id":"v/m","context_length":4096,"top_provider":{"max_completion_tokens":100},"pricing":{"prompt":"3","completion":"3","input_cache_write":"1e400","overrides":[{"min_prompt_tokens":1,"input_cache_write":"0"}]}},
                    {"id":"v/large","context_length":4096,"top_provider":{"max_completion_tokens":100},"pricing":{"prompt":"1","overrides":tiers}},
                    {"id":"v/tiers","context_length":4096,"top_provider":{"max_completion_tokens":100},"pricing":{"prompt":"1","overrides":&tiers[..64]}}
                ]}).to_string())
            }
        }))
        .route("/models/v/m/endpoints", axum::routing::get(|| async {
            axum::Json(json!({"endpoints":[
                {"provider_name":"Incomplete","status":0,"pricing":{"prompt":"0"}},
                {"provider_name":"Unknown","status":null,"pricing":{"prompt":"0","completion":"0","request":"0"}},
                {"provider_name":"C\u{202e}lean","status":0,"context_length":0,"max_completion_tokens":0,"pricing":{"prompt":"3","completion":"3","request":"0","input_cache_write":"1e400"}}
            ]}))
        }));
    let (catalog_url, catalog_task) = mock(catalog).await;
    let catalog = Arc::new(OpenRouterCatalog::for_test(
        catalog_url,
        std::time::Duration::from_secs(2),
    ));
    let (connection, model, deployment, large, tiered) = (
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
    );
    sqlx::query("INSERT INTO provider_connections(id,name,provider,credential_ref,endpoint,enabled) VALUES($1,'Preview','openrouter','env:NOT_USED','https://openrouter.ai/api/v1',false)").bind(connection).execute(&f.store.pool).await.unwrap();
    sqlx::query("INSERT INTO models(id,public_name,supported_protocols) VALUES($1,'preview',ARRAY['chat_completions'])").bind(model).execute(&f.store.pool).await.unwrap();
    sqlx::query("INSERT INTO deployments(id,model_id,provider_connection_id,upstream_model) VALUES($1,$3,$4,'v/m'),($2,$3,$4,'v/large'),($5,$3,$4,'v/tiers')").bind(deployment).bind(large).bind(model).bind(connection).bind(tiered).execute(&f.store.pool).await.unwrap();
    // Create an actual immutable price first. Preview is read-only even when
    // parsing fails or refuses a hostile entry.
    let admin = user(f.owner, true);
    assert_eq!(
        call(
            &f,
            &admin,
            "POST",
            &format!("/api/v1/platform/deployments/{deployment}/prices"),
            price()
        )
        .await
        .0,
        StatusCode::OK
    );
    let before: Vec<Value> =
        sqlx::query_scalar("SELECT to_jsonb(p) FROM deployment_prices p ORDER BY id")
            .fetch_all(&f.store.pool)
            .await
            .unwrap();
    let app = platform_routes()
        .layer(Extension(crate::identity::ManagementPrincipal::Browser(
            admin.clone(),
        )))
        .layer(Extension(admin))
        .layer(Extension(catalog.clone()))
        .with_state(f.store.clone());
    let (gateway_url, gateway_task) = mock(app).await;
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let url = format!("{gateway_url}/api/v1/platform/deployments/{deployment}/price-suggestion");
    let r = client.get(&url).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::BAD_GATEWAY);
    assert_eq!(
        r.json::<Value>().await.unwrap()["error"]["message"],
        "OpenRouter catalog unavailable"
    );
    let r = client.get(&url).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let out: Value = r.json().await.unwrap();
    assert_eq!(out["endpoint"]["selection"], "cheapest");
    assert_eq!(out["endpoint"]["provider"], "Clean");
    assert_eq!(out["draft"]["output_token_limit"], 100);
    assert_eq!(out["draft"]["input_token_limit"], 4096);
    let lines = out["draft"]["price_lines"].as_array().unwrap();
    assert!(
        lines
            .iter()
            .any(|p| p["meter"] == "cache_write_tokens" && p["unknown"] == true)
    );
    assert!(
        out["lines"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["meter"] == "cache_write_tokens" && p["min_prompt_tokens"] == 1)
    );
    assert_eq!(out["needs_review"], true);
    for refused in [large, tiered] {
        let r = client
            .get(format!(
                "{gateway_url}/api/v1/platform/deployments/{refused}/price-suggestion"
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        let bytes = r.bytes().await.unwrap();
        assert!(bytes.len() < 16_384);
        let out: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(out["needs_review"], true);
        assert!(
            out["draft"]["price_lines"]
                .as_array()
                .unwrap()
                .iter()
                .all(|p| p["unknown"] == true)
        );
        assert_eq!(out["draft"]["input_token_limit"], 4096);
        assert_eq!(out["draft"]["output_token_limit"], 100);
        assert!(out.get("endpoint").is_none());
        assert!(!out["warnings"].to_string().contains("price was used"));
    }
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    let r = client
        .get(format!(
            "{gateway_url}/api/v1/platform/deployments/{}/price-suggestion",
            f.deployment
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::CONFLICT);
    let r = client
        .get(format!(
            "{gateway_url}/api/v1/platform/deployments/{}/price-suggestion",
            Uuid::new_v4()
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
    let member = user(f.other, false);
    let app = platform_routes()
        .layer(Extension(crate::identity::ManagementPrincipal::Browser(
            member.clone(),
        )))
        .layer(Extension(member))
        .layer(Extension(catalog))
        .with_state(f.store.clone());
    let (denied_url, denied_task) = mock(app).await;
    let r = client
        .get(format!(
            "{denied_url}/api/v1/platform/deployments/{deployment}/price-suggestion"
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::FORBIDDEN);
    let after: Vec<Value> =
        sqlx::query_scalar("SELECT to_jsonb(p) FROM deployment_prices p ORDER BY id")
            .fetch_all(&f.store.pool)
            .await
            .unwrap();
    assert_eq!(after, before);
    denied_task.abort();
    gateway_task.abort();
    catalog_task.abort();
}
