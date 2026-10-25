//! Native Messages -> real PostgreSQL admission/settlement -> Anthropic HTTP mock.
use super::{tests::db::*, *};
use crate::{
    inference::{Engine, EngineLimits},
    providers::{
        ProviderRegistry,
        anthropic::AnthropicAdapter,
        secrets::{Secret, SecretResolver},
    },
};
use axum::{Json, Router, http::HeaderMap};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

struct Resolver;
impl SecretResolver for Resolver {
    fn resolve(&self, _: &str) -> Result<Secret, InferenceError> {
        Secret::new("server-provider-key".into())
    }
}
struct Servers(Vec<tokio::task::JoinHandle<()>>);
impl Drop for Servers {
    fn drop(&mut self) {
        for task in &self.0 {
            task.abort();
        }
    }
}

async fn publish(f: &Fixture, version: i32, denied: Option<&str>) -> Uuid {
    if version == 1 {
        return f.price(1_000_000).await;
    }
    let mut rates = rates();
    if denied == Some("5m") {
        rates.write_5m = CacheRate::NotApplicable;
    }
    if denied == Some("1h") {
        rates.write_1h = CacheRate::NotApplicable;
    }
    if denied == Some("unknown-read") {
        rates.read = CacheRate::Unknown;
    }
    if denied == Some("unknown-5m") {
        rates.write_5m = CacheRate::Unknown;
    }
    if version == 2 {
        return f.v2(rates).await;
    }
    let mut lines = vec![
        json!({"meter":"input_tokens","microusd_per_batch":"1000000","batch":1000000,"unit_label":"/M","sku_label":"input"}),
        json!({"meter":"output_tokens","microusd_per_batch":"1000000","batch":1000000,"unit_label":"/M","sku_label":"output"}),
    ];
    for (meter, rate) in [
        "cache_read_tokens",
        "cache_write_tokens",
        "cache_write_5m_tokens",
        "cache_write_1h_tokens",
    ]
    .into_iter()
    .zip(rates.rates())
    {
        lines.push(match rate {
            CacheRate::Priced { microusd_per_million } => json!({"meter":meter,"microusd_per_batch":microusd_per_million.to_string(),"batch":1000000,"unit_label":"/M","sku_label":"cache"}),
            CacheRate::NotApplicable => json!({"meter":meter,"not_applicable":true}),
            CacheRate::Unknown => unreachable!(),
        });
    }
    lines.push(json!({"meter":"requests","microusd_per_batch":"0","batch":1,"unit_label":"/request","sku_label":"request"}));
    for meter in [
        "output_images",
        "input_characters",
        "input_audio_seconds_ms",
        "output_audio_seconds_ms",
        "search_units",
    ] {
        lines.push(json!({"meter":meter,"not_applicable":true}));
    }
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO deployment_prices(id,deployment_id,input_token_limit,output_token_limit,pricing_version,price_lines,max_units) VALUES($1,$2,100,50,3,$3,'{}')")
        .bind(id).bind(f.deployment).bind(json!(lines)).execute(&f.store.pool).await.unwrap();
    id
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn native_cache_requires_pinned_ttl_prices_before_reserving_or_dispatching(
    pool: sqlx::PgPool,
) {
    let f = fixture(pool).await;
    sqlx::query("UPDATE provider_connections SET provider='anthropic'")
        .execute(&f.store.pool)
        .await
        .unwrap();
    sqlx::query("UPDATE models SET supported_protocols=ARRAY['messages']")
        .execute(&f.store.pool)
        .await
        .unwrap();
    let key = crate::auth::NewApiKey::generate().unwrap();
    sqlx::query("INSERT INTO api_keys(id,workspace_id,issued_to_user_id,name,secret_hash) VALUES($1,$2,$3,'cache test',$4)")
        .bind(key.id).bind(f.principal.workspace_id).bind(f.owner).bind(key.digest.as_slice()).execute(&f.store.pool).await.unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let capture = seen.clone();
    let db = f.store.pool.clone();
    let upstream = Router::new().fallback(move |headers: HeaderMap, Json(body): Json<Value>| {
        let db = db.clone(); let capture = capture.clone();
        async move {
            assert_eq!(headers["x-api-key"], "server-provider-key");
            let reservation: (Uuid, i64) = sqlx::query_as("SELECT price_id,held_microusd FROM governance_reservations WHERE state='pending' ORDER BY admitted_at DESC LIMIT 1").fetch_one(&db).await.unwrap();
            let blocks = body["system"].as_array().unwrap();
            let five = blocks.iter().any(|b| b["cache_control"]["ttl"] != "1h");
            let hour = blocks.iter().any(|b| b["cache_control"]["ttl"] == "1h");
            let (five_tokens, hour_tokens) = (if five {3} else {0}, if hour {4} else {0});
            capture.lock().unwrap().push(reservation);
            Json(json!({"id":"upstream","type":"message","role":"assistant","model":"private","content":[{"type":"text","text":"ok"}],"stop_reason":"end_turn","stop_sequence":null,
                "usage":{"input_tokens":2,"output_tokens":1,"cache_read_input_tokens":0,"cache_creation_input_tokens":five_tokens+hour_tokens,"cache_creation":{"ephemeral_5m_input_tokens":five_tokens,"ephemeral_1h_input_tokens":hour_tokens}}}))
        }
    });
    let mut servers = Servers(vec![]);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/v1", listener.local_addr().unwrap());
    servers.0.push(tokio::spawn(async move {
        axum::serve(listener, upstream).await.unwrap();
    }));
    let mut registry = ProviderRegistry::default();
    registry
        .register(Arc::new(AnthropicAdapter::for_test(
            Arc::new(Resolver),
            base,
        )))
        .unwrap();
    let engine = Engine::new(Arc::new(f.store.clone()), registry, EngineLimits::default()).unwrap();
    let app = crate::http::router_with_engine(f.store.clone(), None, engine);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    servers.0.push(tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    }));
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    // Every refusal must precede *both* the upstream call and the reservation.
    // Mixed TTLs must require both categories, including implicit 5-minute TTL.
    for (version, denied, ttls, accepted) in [
        (1, None, vec![Some("5m")], false),
        (1, None, vec![Some("1h")], false),
        (2, Some("5m"), vec![None], false),
        (2, Some("1h"), vec![Some("1h")], false),
        (2, Some("unknown-5m"), vec![Some("5m")], false),
        (2, Some("unknown-read"), vec![Some("1h")], false),
        (3, Some("5m"), vec![Some("5m")], false),
        (3, Some("1h"), vec![Some("5m"), Some("1h")], false),
        (2, None, vec![None], true),
        (2, None, vec![Some("5m")], true),
        (2, None, vec![Some("1h")], true),
        (3, None, vec![Some("5m")], true),
        (3, None, vec![Some("1h")], true),
        (3, None, vec![Some("5m"), Some("1h")], true),
    ] {
        let price = publish(&f, version, denied).await;
        let before = seen.lock().unwrap().len();
        let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM governance_reservations")
            .fetch_one(&f.store.pool)
            .await
            .unwrap();
        let blocks: Vec<_> = ttls
            .iter()
            .map(|ttl| {
                let mut block =
                    json!({"type":"text","text":"cache me","cache_control":{"type":"ephemeral"}});
                if let Some(ttl) = ttl {
                    block["cache_control"]["ttl"] = json!(ttl);
                }
                block
            })
            .collect();
        let response = client.post(format!("{base}/v1/messages")).bearer_auth(&key.token).header("anthropic-version","2023-06-01")
            .json(&json!({"model":"company/smart","max_tokens":10,"system":blocks,"messages":[{"role":"user","content":"hello"}]})).send().await.unwrap();
        let status = response.status();
        let body: Value = response.json().await.unwrap();
        assert_eq!(
            status.as_u16(),
            if accepted { 200 } else { 400 },
            "v{version} {ttls:?}: {body}"
        );
        let after: i64 = sqlx::query_scalar("SELECT count(*) FROM governance_reservations")
            .fetch_one(&f.store.pool)
            .await
            .unwrap();
        if !accepted {
            assert!(
                body["error"]["message"]
                    .as_str()
                    .unwrap()
                    .contains("cache_control")
            );
            assert_eq!(after, rows);
            assert_eq!(seen.lock().unwrap().len(), before);
        } else {
            assert_eq!(after, rows + 1);
            let has_hour = ttls.contains(&Some("1h"));
            {
                let seen = seen.lock().unwrap();
                assert_eq!(seen.len(), before + 1);
                assert_eq!(
                    seen[before].0, price,
                    "the checked price is the pinned price"
                );
                assert!(
                    seen[before].1 >= if has_hour { 210 } else { 135 },
                    "full input at the highest requested write rate plus output"
                );
            }
            let (state, actual): (String, Option<i64>) = sqlx::query_as(
                "SELECT state,actual_microusd FROM governance_reservations WHERE price_id=$1",
            )
            .bind(price)
            .fetch_one(&f.store.pool)
            .await
            .unwrap();
            let has_five = ttls.iter().any(|t| *t != Some("1h"));
            assert_eq!(state, "settled");
            assert_eq!(
                actual,
                Some(3 + if has_five { 4 } else { 0 } + if has_hour { 8 } else { 0 }),
                "write aggregate is not charged in addition to TTL allocations"
            );
        }
    }
}
