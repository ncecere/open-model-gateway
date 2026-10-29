//! Slice 3's executed, raw-wire registry: a real gateway and disposable PostgreSQL.
use super::*;
use openapi::responses::SLICE3_AREAS;

struct Journey<'a> {
    client: reqwest::Client,
    base: String,
    actor: &'a Browser,
    observed: PositiveFixtures,
}
impl Journey<'_> {
    async fn request(&mut self, method: &str, path: &str, input: Option<Value>) -> Value {
        let r = send(
            &self.client,
            &self.base,
            Some(self.actor),
            method,
            path,
            input,
        )
        .await;
        assert!(
            r.status.is_success(),
            "{method} {path}: {} {:?}",
            r.status,
            r.bytes
        );
        self.observed.observe(&r);
        let value: Value = serde_json::from_slice(&r.bytes).unwrap();
        if let Some(data) = value.get("data") {
            assert!(
                !data.as_array().unwrap().is_empty(),
                "populated fixture {path}"
            );
        }
        assert!(!value.to_string().contains("env:CONTRACT_REFERENCE"));
        value
    }
}
fn legacy_price() -> Value {
    json!({"input_microusd_per_million":"9007199254740993","output_microusd_per_million":"1","input_token_limit":1,"output_token_limit":1})
}
fn v3_price() -> Value {
    let mut lines = vec![
        json!({"meter":"input_tokens","microusd_per_batch":"123","batch":1000000,"unit_label":"/M tokens","sku_label":"Input"}),
        json!({"meter":"output_tokens","microusd_per_batch":"456","batch":1000000,"unit_label":"/M tokens","sku_label":"Output"}),
    ];
    for meter in [
        "cache_read_tokens",
        "cache_write_tokens",
        "cache_write_5m_tokens",
        "cache_write_1h_tokens",
        "output_images",
        "input_characters",
        "input_audio_seconds_ms",
        "output_audio_seconds_ms",
        "search_units",
        "requests",
    ] {
        lines.push(json!({"meter":meter,"not_applicable":true}));
    }
    json!({"pricing_version":3,"input_token_limit":100,"output_token_limit":10,"price_lines":lines,"max_units":{"requests":"1"}})
}
/// The twelve meters every price must state (`Meter::ALL`).
const METERS: [&str; 12] = [
    "input_tokens",
    "output_tokens",
    "cache_read_tokens",
    "cache_write_tokens",
    "cache_write_5m_tokens",
    "cache_write_1h_tokens",
    "output_images",
    "input_characters",
    "input_audio_seconds_ms",
    "output_audio_seconds_ms",
    "search_units",
    "requests",
];
/// A priced line: `(meter, batch)` fixes the canonical unit label.
fn line(meter: &str, batch: u64, amount: u64) -> Value {
    let label = match (meter, batch) {
        ("output_images", _) => "/image",
        ("input_characters", _) => "/M characters",
        ("search_units", _) => "/search",
        ("requests", _) => "/request",
        (_, 1000) => "/second",
        (_, 60000) => "/minute",
        (_, 3600000) => "/hour",
        _ => "/M tokens",
    };
    json!({"meter":meter,"microusd_per_batch":amount.to_string(),"batch":batch,"unit_label":label,"sku_label":format!("SKU {meter}")})
}
fn tiered(mut l: Value, min: u64) -> Value {
    l["min_prompt_tokens"] = json!(min);
    l
}
fn varied(mut l: Value, variant: &str) -> Value {
    l["variant"] = json!(variant);
    l
}
/// A v3 price stating all twelve meters: the given priced lines, the rest not
/// applicable. The batch list mirrors the same meters at other amounts.
fn v3_with(priced: &[Value], extra: &[Value], max_units: Value) -> Value {
    let build = |bump: u64| {
        let mut lines: Vec<Value> = METERS
            .iter()
            .filter(|m| !priced.iter().any(|p| p["meter"] == **m))
            .map(|m| json!({"meter":m,"not_applicable":true}))
            .collect();
        for l in priced.iter().chain(extra) {
            let mut l = l.clone();
            let amount: u64 = l["microusd_per_batch"].as_str().unwrap().parse().unwrap();
            l["microusd_per_batch"] = json!((amount + bump).to_string());
            lines.push(l);
        }
        lines
    };
    json!({"pricing_version":3,"input_token_limit":100,"output_token_limit":10,"price_lines":build(0),"batch_price_lines":build(1),"max_units":max_units})
}
/// Observes every view of the route's latest price: the deployment detail, and
/// the first-route price of the platform and workspace model comparisons.
async fn price_views<'a>(
    j: &mut Journey<'a>,
    admin: &'a Browser,
    owner: &'a Browser,
    team: Uuid,
    deployment: Uuid,
    models: (Uuid, Uuid),
) {
    let saved = j.actor;
    j.actor = admin;
    j.request(
        "GET",
        &format!("/api/v1/platform/deployments/{deployment}"),
        None,
    )
    .await;
    j.request(
        "GET",
        &format!(
            "/api/v1/platform/models/compare?ids={},{}",
            models.0, models.1
        ),
        None,
    )
    .await;
    j.actor = owner;
    j.request(
        "GET",
        &format!(
            "/api/v1/workspaces/{team}/models/compare?ids={},{}",
            models.0, models.1
        ),
        None,
    )
    .await;
    j.actor = saved;
}
/// A telemetry attempt exactly as the engine records it: the execution row with
/// its timings and the governance reservation (settled with an actual cost, or
/// unknown with only a retained hold). The maintaining triggers fill the totals.
#[allow(clippy::too_many_arguments)]
async fn seed_attempt(
    pool: &PgPool,
    ws: Uuid,
    key: Uuid,
    deployment: Uuid,
    model: &str,
    state: &str,
    actual: Option<i64>,
    held: Option<i64>,
) {
    let id = Uuid::new_v4();
    let tokens = actual.map(|_| (10_i64, 5_i64));
    sqlx::query("INSERT INTO inference_executions(id,workspace_id,api_key_id,deployment_id,public_model,provider,streamed,state,started_at,completed_at,elapsed_ms,root_request_id,attempt_number,input_tokens,output_tokens,finish_reason,time_to_first_token_ms,generation_ms) VALUES($1,$2,$3,$4,$5,'openrouter',true,$6,now()-interval '1 second',now()-interval '1 second'+interval '600 milliseconds',600,$1,1,$7,$8,'stop',100,600)")
        .bind(id).bind(ws).bind(key).bind(deployment).bind(model).bind(state)
        .bind(tokens.map(|t| t.0)).bind(tokens.map(|t| t.1))
        .execute(pool).await.unwrap();
    let reservation = if actual.is_some() {
        "settled"
    } else {
        "unknown"
    };
    sqlx::query("INSERT INTO governance_reservations(execution_id,workspace_id,api_key_id,deployment_id,admitted_at,minute_start,month_start,lease_expires_at,state,actual_microusd,held_microusd,unbounded_cost,input_tokens,output_tokens) SELECT $1,$2,$3,$4,e.started_at,date_trunc('minute',e.started_at,'UTC'),date_trunc('month',e.started_at,'UTC'),e.started_at,$5,$6,$7,$8,$9,$10 FROM inference_executions e WHERE e.id=$1")
        .bind(id).bind(ws).bind(key).bind(deployment).bind(reservation).bind(actual).bind(held)
        .bind(actual.is_none() && held.is_none())
        .bind(tokens.map(|t| t.0)).bind(tokens.map(|t| t.1))
        .execute(pool).await.unwrap();
}
/// A legacy local or non-chat route: model and route inserted as an earlier
/// release accepted them (route validation did not exist or had other rules).
async fn legacy_route(
    pool: &PgPool,
    provider: Uuid,
    public_name: &str,
    protocols: &[&str],
    upstream: &str,
) -> (Uuid, Uuid) {
    let (model, deployment) = (Uuid::new_v4(), Uuid::new_v4());
    sqlx::query("INSERT INTO models(id,public_name,supported_protocols) VALUES($1,$2,$3)")
        .bind(model)
        .bind(public_name)
        .bind(
            protocols
                .iter()
                .map(|p| (*p).to_owned())
                .collect::<Vec<_>>(),
        )
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO deployments(id,model_id,provider_connection_id,upstream_model,enabled) VALUES($1,$2,$3,$4,true)")
        .bind(deployment)
        .bind(model)
        .bind(provider)
        .bind(upstream)
        .execute(pool)
        .await
        .unwrap();
    (model, deployment)
}
/// A policy body with every limit supplied (tighten-only: local <= platform).
#[allow(clippy::too_many_arguments)]
fn limits(
    rpm: i64,
    tpm: i64,
    concurrent: i64,
    jobs: i64,
    storage: i64,
    holder: i64,
    workspace: i64,
    budgets: Value,
) -> Value {
    json!({"requests_per_minute":rpm,"tokens_per_minute":tpm,"concurrent_requests":concurrent,"concurrent_jobs":jobs,"storage_bytes":storage,"key_issuances_per_holder":holder,"key_issuances_per_workspace":workspace,"budgets":budgets})
}
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn management_openapi_slice3_fixtures(pool: PgPool) {
    let mut f = fixture(&pool).await;
    f.s = f.s.with_secret_allowlists(
        crate::providers::secrets::EnvSecrets::from_lookup(["CONTRACT_REFERENCE".into()], |_| None),
        Default::default(),
    );
    let admin = browser(&pool, &f.admin).await;
    let owner = browser(&pool, &f.owner).await;
    let member = browser(&pool, &f.member).await;
    let auditor = browser(&pool, &f.auditor).await;
    let mock = crate::identity::tests::MockProvider::start(None).await;
    let identity = mock.state(pool.clone()).await;
    let engine = crate::inference::Engine::new(
        std::sync::Arc::new(f.s.clone()),
        Default::default(),
        Default::default(),
    )
    .unwrap();
    // The OpenRouter public catalog, one model per drafting branch. `contract/model`
    // lists endpoints (the endpoint-priced branch); the others have none, so the
    // catalog's top-provider price is used. Tiers and cache keys, missing
    // ceilings and prices, and each non-chat workload's units are all real
    // catalog shapes.
    let catalog_models = json!({"data":[
        {"id":"contract/model","context_length":8192,"top_provider":{"max_completion_tokens":1024},"pricing":{"prompt":"0.000000123","completion":"0.000000456"}},
        {"id":"contract/catalog-only","context_length":8192,"top_provider":{"max_completion_tokens":1024},"pricing":{"prompt":"0.000000123","completion":"0.000000456","input_cache_read":"0.00000001","input_cache_write":"0.000000125","input_cache_write_1h":"0.0000002","overrides":[{"min_prompt_tokens":4000,"prompt":"0.0000005","completion":"0.0000025","input_cache_read":"0.00000005","input_cache_write":"0.000000625","input_cache_write_1h":"0.000001"}]}},
        {"id":"contract/bare","pricing":{}},
        {"id":"contract/embed","context_length":8192,"pricing":{"prompt":"0.00000002"}},
        {"id":"contract/image","pricing":{}},
        {"id":"contract/stt-minute","pricing":{"prompt":"0.0001","completion":"0.00001"}},
        {"id":"contract/stt-hour","pricing":{"prompt":"0.0000000025"}},
        {"id":"contract/tts-minute","pricing":{"prompt":"0.000015","completion":"0.0001"}},
        {"id":"contract/tts-hour","pricing":{"prompt":"0.000015","completion":"0.0000000025"}}
    ]});
    let model_endpoints = json!({"data":{"id":"contract/model","endpoints":[{"provider_name":"Contract Cheap","status":0,"pricing":{"prompt":"0.000000123","completion":"0.000000456"}},{"provider_name":"Contract Dear","status":0,"pricing":{"prompt":"0.000000246","completion":"0.000000912"}}]}});
    let image_endpoints = json!({"data":{"endpoints":[{"pricing":[{"billable":"output_image","unit":"image","cost_usd":0.041,"variant":"768"},{"billable":"output_image","unit":"image","cost_usd":0.06,"variant":"1024"}]}]}});
    let catalog_app = axum::Router::new()
        .route(
            "/models",
            axum::routing::get(move || {
                let body = catalog_models.clone();
                async move { Json(body) }
            }),
        )
        .route(
            "/models/contract/model/endpoints",
            axum::routing::get(move || {
                let body = model_endpoints.clone();
                async move { Json(body) }
            }),
        )
        .route(
            "/images/models/contract/image/endpoints",
            axum::routing::get(move || {
                let body = image_endpoints.clone();
                async move { Json(body) }
            }),
        );
    let (catalog_url, catalog_task) = governance::suggestion::tests::mock(catalog_app).await;
    let _catalog_server = Server(catalog_task);
    let catalog = std::sync::Arc::new(governance::suggestion::OpenRouterCatalog::for_test(
        catalog_url,
        std::time::Duration::from_secs(1),
    ));
    let app = crate::http::router_with_identity(f.s.clone(), None, engine, identity)
        .layer(Extension(catalog));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let _server = Server(tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap()
    }));
    let mut j = Journey {
        client: reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap(),
        base,
        actor: &admin,
        observed: Default::default(),
    };
    let ws_policy = format!("/api/v1/workspaces/{}/policy", f.team);
    // Policy layers, state A: nothing configured anywhere (every limit absent).
    for who in [&owner, &member] {
        j.actor = who;
        j.request("GET", &ws_policy, None).await;
    }
    j.actor = &admin;
    let cat = id(&j
        .request(
            "POST",
            "/api/v1/platform/catalogs",
            Some(json!({"name":"Contract catalog","description":"Live defaults"})),
        )
        .await);
    let provider = id(&j.request("POST", "/api/v1/platform/providers", Some(json!({"name":"Contract reference","provider":"openrouter","credential_ref":"env:CONTRACT_REFERENCE","enabled":true}))).await);
    // A connection nothing uses, so its deletion is observed without touching the route fixtures.
    let disposable = id(&j.request("POST", "/api/v1/platform/providers", Some(json!({"name":"Contract disposable","provider":"openrouter","credential_ref":"env:CONTRACT_REFERENCE","enabled":false}))).await);
    // Identity-bearing provider kinds: Bedrock default and role identities (non-null
    // `aws_auth`), created through the real route. A named AWS profile needs a
    // server allowlist and a credential-less (`none`) local endpoint needs a
    // process-level GATEWAY_LOCAL_UPSTREAMS approval; the test process sets
    // neither, so those connections are inserted directly (the same way the
    // batch-scheduling tests seed local routes).
    let bedrock_default = id(&j.request("POST", "/api/v1/platform/providers", Some(json!({"name":"Contract Bedrock","provider":"bedrock","credential_ref":"aws:default","endpoint":null,"region":"us-east-1","enabled":false}))).await);
    j.request("POST", "/api/v1/platform/providers", Some(json!({"name":"Contract Bedrock role","provider":"bedrock","credential_ref":"aws:role:arn:aws:iam::123456789012:role/gateway/bedrock-invoke","aws_external_id":"tenant-ext-42","aws_session_name":"gateway.contract","endpoint":null,"region":"us-east-1","enabled":false}))).await;
    let local_provider = Uuid::new_v4();
    let vllm_provider = Uuid::new_v4();
    for (connection, name, kind, reference, endpoint, region) in [
        (
            Uuid::new_v4(),
            "Contract profile",
            "bedrock",
            "aws:profile:contract",
            None,
            Some("us-east-1"),
        ),
        (
            local_provider,
            "Contract local",
            "ollama",
            "none",
            Some("http://10.0.0.5:11434/v1"),
            None,
        ),
        (
            vllm_provider,
            "Contract vLLM",
            "vllm",
            "none",
            Some("http://10.0.0.5:8000/v1"),
            None,
        ),
    ] {
        sqlx::query("INSERT INTO provider_connections(id,name,provider,credential_ref,endpoint,region,enabled) VALUES($6,$1,$2,$3,$4,$5,false)")
            .bind(name)
            .bind(kind)
            .bind(reference)
            .bind(endpoint)
            .bind(region)
            .bind(connection)
            .execute(&pool)
            .await
            .unwrap();
    }
    let model = id(&j.request("POST", "/api/v1/platform/models", Some(json!({"public_name":"contract/model","display_name":"Contract model","enabled":true}))).await);
    let unpriced = id(&j.request("POST", "/api/v1/platform/models", Some(json!({"public_name":"contract/unpriced","display_name":"Contract unpriced","enabled":true}))).await);
    let deployment = id(&j.request("POST", "/api/v1/platform/deployments", Some(json!({"model_id":model,"provider_connection_id":provider,"upstream_model":"contract/model","enabled":true}))).await);
    // Routing is configured first: `deployment` is the model's first route.
    j.request("PUT", &format!("/api/v1/platform/deployments/{deployment}/routing"), Some(json!({"priority":-1,"weight":1,"residency":null,"failure_threshold":3,"cooldown_seconds":30}))).await;
    j.request("PUT", &format!("/api/v1/platform/models/{model}/routing"), Some(json!({"strategy":"priority","max_attempts":2,"allow_ambiguous_failover":false,"required_residency":null}))).await;
    // Second model through the one-call setup, in the catalog (workspace-visible).
    let setup = j.request("POST", "/api/v1/platform/model-setup", Some(json!({"model":{"public_name":"contract/second","display_name":"Second model","enabled":true},"route":{"provider_connection_id":provider,"upstream_model":"contract/model","enabled":true},"price":v3_price(),"catalog_ids":[cat]}))).await;
    let second = Uuid::parse_str(setup["model_id"].as_str().unwrap()).unwrap();
    // A setup without a price publishes none (price_id null).
    let unpriced_setup = j.request("POST", "/api/v1/platform/model-setup", Some(json!({"model":{"public_name":"contract/third","display_name":"Third model","enabled":true},"route":{"provider_connection_id":provider,"upstream_model":"contract/third","enabled":true}}))).await;
    assert!(unpriced_setup["price_id"].is_null());
    // Legacy routes (accepted before route validation): chat without any catalog
    // data, and one non-serving video route that also anchors the telemetry.
    let (_, bare) = legacy_route(
        &pool,
        provider,
        "contract/bare-model",
        &["chat_completions"],
        "contract/bare",
    )
    .await;
    let (legacy_model, anchor) = legacy_route(
        &pool,
        provider,
        "contract/legacy-video",
        &["videos"],
        "contract/legacy",
    )
    .await;
    let mut drafted = vec![bare];
    for (name, protocols, upstream) in [
        (
            "contract/embed-model",
            &["embeddings"][..],
            "contract/embed",
        ),
        ("contract/image-model", &["images"][..], "contract/image"),
        (
            "contract/stt-minute-model",
            &["audio_transcriptions"][..],
            "contract/stt-minute",
        ),
        (
            "contract/stt-hour-model",
            &["audio_transcriptions"][..],
            "contract/stt-hour",
        ),
        (
            "contract/tts-minute-model",
            &["audio_speech"][..],
            "contract/tts-minute",
        ),
        (
            "contract/tts-hour-model",
            &["audio_speech"][..],
            "contract/tts-hour",
        ),
    ] {
        drafted.push(
            legacy_route(&pool, provider, name, protocols, upstream)
                .await
                .1,
        );
    }
    j.request(
        "PATCH",
        &format!("/api/v1/platform/catalogs/{cat}"),
        Some(json!({"name":"Contract renamed","description":"Renamed catalog"})),
    )
    .await;
    j.request(
        "PATCH",
        &format!("/api/v1/platform/providers/{provider}"),
        Some(json!({"enabled":true})),
    )
    .await;
    j.request(
        "PATCH",
        &format!("/api/v1/platform/models/{model}"),
        Some(json!({"description":"Description"})),
    )
    .await;
    j.request(
        "PUT",
        &format!("/api/v1/platform/catalogs/{cat}/models"),
        Some(json!({"model_ids":[model,second,unpriced]})),
    )
    .await;
    j.request(
        "PUT",
        &format!("/api/v1/platform/models/{model}/catalogs"),
        Some(json!({"catalog_ids":[cat]})),
    )
    .await;
    j.request(
        "PUT",
        "/api/v1/platform/catalog-defaults",
        Some(json!({"personal":[cat],"team":[cat],"project":[cat]})),
    )
    .await;
    // Policy layers, state B: the platform default for teams is set.
    let r = send(
        &j.client,
        &j.base,
        Some(&admin),
        "PUT",
        "/api/v1/platform/workspace-types/team/policy",
        Some(limits(
            100,
            100_000,
            10,
            5,
            1_000_000,
            50,
            500,
            json!([{"period":"month","amount_microusd":"9007199254740993"}]),
        )),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK);
    for who in [&owner, &member] {
        j.actor = who;
        j.request("GET", &ws_policy, None).await;
    }
    j.actor = &admin;
    // The price history of the route, every immutable version shape, with each
    // version's views (route detail, platform and workspace comparison) while it
    // is the latest: legacy and v2 (all cache-rate states), then v3 with every
    // meter and unit, then v3 with mostly not-applicable meters.
    let models = (model, second);
    price_views(&mut j, &admin, &owner, f.team, deployment, models).await;
    j.request(
        "POST",
        &format!("/api/v1/platform/deployments/{deployment}/prices"),
        Some(legacy_price()),
    )
    .await;
    price_views(&mut j, &admin, &owner, f.team, deployment, models).await;
    let cache = |read: Value, write: Value, write_5m: Value, write_1h: Value| json!({"pricing_version":2,"input_microusd_per_million":"1","output_microusd_per_million":"2","input_token_limit":100,"output_token_limit":10,"cache_pricing":{"read":read,"write":write,"write_5m":write_5m,"write_1h":write_1h}});
    let priced = |amount: &str| json!({"status":"priced","microusd_per_million":amount});
    let unknown = || json!({"status":"unknown"});
    let na = || json!({"status":"not_applicable"});
    for body in [
        cache(priced("1"), unknown(), na(), priced("2")),
        cache(unknown(), na(), priced("3"), unknown()),
        cache(na(), priced("4"), unknown(), na()),
    ] {
        j.request(
            "POST",
            &format!("/api/v1/platform/deployments/{deployment}/prices"),
            Some(body),
        )
        .await;
        price_views(&mut j, &admin, &owner, f.team, deployment, models).await;
    }
    let rich = v3_with(
        &[
            line("input_tokens", 1_000_000, 123_000),
            line("output_tokens", 1_000_000, 456_000),
            line("cache_read_tokens", 1_000_000, 12_000),
            line("cache_write_tokens", 1_000_000, 150_000),
            line("cache_write_5m_tokens", 1_000_000, 160_000),
            line("cache_write_1h_tokens", 1_000_000, 240_000),
            line("output_images", 1, 40_000),
            line("input_characters", 1_000_000, 15),
            line("input_audio_seconds_ms", 1000, 5),
            line("output_audio_seconds_ms", 1000, 6),
            line("search_units", 1, 7),
            line("requests", 1, 1),
        ],
        &[
            tiered(line("input_tokens", 1_000_000, 150_000), 4000),
            varied(line("output_images", 1, 50_000), "1024"),
            tiered(line("input_audio_seconds_ms", 60_000, 300), 10),
            tiered(line("input_audio_seconds_ms", 3_600_000, 18_000), 20),
            tiered(line("output_audio_seconds_ms", 60_000, 360), 10),
            tiered(line("output_audio_seconds_ms", 3_600_000, 21_600), 20),
            line("input_audio_tokens", 1_000_000, 100),
            line("cache_read_audio_tokens", 1_000_000, 10),
            line("output_audio_tokens", 1_000_000, 200),
            varied(line("output_video_seconds_ms", 1000, 9), "720p"),
            varied(line("output_video_seconds_ms", 60_000, 500), "1080p"),
            varied(line("output_video_seconds_ms", 3_600_000, 30_000), "4k"),
        ],
        json!({"requests":"1","output_images":"4","input_characters":"10000","input_audio_seconds_ms":"60000","output_audio_seconds_ms":"60000","search_units":"2","output_video_seconds_ms":"10000"}),
    );
    j.request(
        "POST",
        &format!("/api/v1/platform/deployments/{deployment}/prices"),
        Some(rich),
    )
    .await;
    price_views(&mut j, &admin, &owner, f.team, deployment, models).await;
    let sparse = v3_with(
        &[
            line("input_tokens", 1_000_000, 123),
            line("output_tokens", 1_000_000, 456),
            line("requests", 1, 1),
        ],
        &[],
        json!({"requests":"1"}),
    );
    j.request(
        "POST",
        &format!("/api/v1/platform/deployments/{deployment}/prices"),
        Some(sparse),
    )
    .await;
    price_views(&mut j, &admin, &owner, f.team, deployment, models).await;
    j.request(
        "POST",
        &format!("/api/v1/platform/deployments/{deployment}/prices"),
        Some(v3_price()),
    )
    .await;
    let mut mapped_price = v3_price();
    mapped_price["price_lines"][0]["meter"] = json!({"input_tokens":null});
    j.request(
        "POST",
        &format!("/api/v1/platform/deployments/{deployment}/prices"),
        Some(mapped_price),
    )
    .await;
    price_views(&mut j, &admin, &owner, f.team, deployment, models).await;
    // Route and model routing with a residency, then route health as the engine records it.
    j.request("PUT", &format!("/api/v1/platform/models/{model}/routing"), Some(json!({"strategy":"weighted","max_attempts":3,"allow_ambiguous_failover":true,"required_residency":"eu"}))).await;
    j.request(
        "GET",
        &format!("/api/v1/platform/models/{model}/routing"),
        None,
    )
    .await;
    j.request(
        "GET",
        &format!("/api/v1/platform/deployments/{deployment}/routing"),
        None,
    )
    .await;
    j.request("PUT", &format!("/api/v1/platform/deployments/{deployment}/routing"), Some(json!({"priority":-1,"weight":5,"residency":"eu","failure_threshold":3,"cooldown_seconds":30}))).await;
    j.request(
        "GET",
        &format!("/api/v1/platform/deployments/{deployment}/routing"),
        None,
    )
    .await;
    crate::routing::record_result(&f.s, deployment, None)
        .await
        .unwrap();
    j.request(
        "GET",
        &format!("/api/v1/platform/deployments/{deployment}/routing"),
        None,
    )
    .await;
    for _ in 0..3 {
        crate::routing::record_result(
            &f.s,
            deployment,
            Some(crate::inference::error::InferenceError::Timeout),
        )
        .await
        .unwrap();
    }
    j.request(
        "GET",
        &format!("/api/v1/platform/deployments/{deployment}/routing"),
        None,
    )
    .await;
    j.request("PUT", &format!("/api/v1/platform/deployments/{deployment}/batch-scheduling"), Some(json!({"max_concurrency":3,"yield_live_threshold":5,"window":{"timezone":"UTC","days":["mon","fri"],"start":"23:00","end":"06:00"}}))).await;
    // A vLLM-compatible route: defaults, then a priority and window, then
    // server-load gates (written by the production writer: the metrics URL needs
    // a process-level endpoint approval) and the route's batch signals.
    let (_, vllm) = legacy_route(
        &pool,
        vllm_provider,
        "contract/vllm-model",
        &["chat_completions"],
        "contract/vllm",
    )
    .await;
    let schedule = format!("/api/v1/platform/deployments/{vllm}/batch-scheduling");
    j.request("GET", &schedule, None).await;
    j.request("PUT", &schedule, Some(json!({"max_concurrency":4,"yield_live_threshold":2,"priority":10,"window":{"timezone":"America/New_York","days":["mon","tue","wed","thu","fri"],"start":"19:00","end":"07:00"}}))).await;
    j.request("GET", &schedule, None).await;
    let gate = |waiting: Option<i32>, running: Option<i32>, kv: Option<i16>| {
        crate::jobs::schedule::RouteSettings {
            max_concurrency: 4,
            yield_live_threshold: Some(2),
            metrics: Some(crate::jobs::schedule::MetricsGate {
                url: "http://10.0.0.5:8000/metrics".into(),
                max_waiting: waiting,
                max_running: running,
                max_kv_cache_percent: kv,
            }),
            priority: Some(10),
            window: None,
        }
    };
    for (settings, signal) in [
        (
            gate(Some(5), None, Some(90)),
            "INSERT INTO deployment_batch_signals(deployment_id,paused_reason,checked_at,live_in_flight,metrics_checked_at,metrics_ok,metrics_waiting,metrics_running,metrics_kv_cache_permille,metrics_error) VALUES($1,'server_busy',clock_timestamp(),4,clock_timestamp(),true,7,3,950,NULL)",
        ),
        (
            gate(None, Some(8), None),
            "UPDATE deployment_batch_signals SET paused_reason=NULL,checked_at=clock_timestamp(),metrics_checked_at=clock_timestamp(),metrics_ok=false,metrics_waiting=NULL,metrics_running=NULL,metrics_kv_cache_permille=NULL,metrics_error='unreachable' WHERE deployment_id=$1",
        ),
    ] {
        let mut tx = pool.begin().await.unwrap();
        crate::jobs::schedule::save_settings(&mut tx, vllm, &settings, f.admin.user_id)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        sqlx::query(signal).bind(vllm).execute(&pool).await.unwrap();
        j.request("GET", &schedule, None).await;
    }
    sqlx::query("UPDATE deployment_batch_signals SET paused_reason='metrics_unavailable',checked_at=clock_timestamp() WHERE deployment_id=$1").bind(vllm).execute(&pool).await.unwrap();
    j.request("GET", &schedule, None).await;
    // Populated shared preview, and catalog summary's owner-private count.
    direct(&pool, f.team, model).await;
    direct(&pool, f.personal, model).await;
    sqlx::query("INSERT INTO workspace_catalog_overrides(workspace_id) VALUES($1)")
        .bind(f.personal)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO workspace_catalog_overrides(workspace_id) VALUES($1)")
        .bind(f.team)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO workspace_catalog_override_items(workspace_id,catalog_id) VALUES($1,$2),($3,$2)").bind(f.personal).bind(cat).bind(f.team).execute(&pool).await.unwrap();
    let preview = j
        .request(
            "POST",
            &format!("/api/v1/platform/deployments/{deployment}/configuration/preview"),
            Some(json!({"enabled":false})),
        )
        .await;
    assert_eq!(preview["affected_workspaces"]["total"], 1);
    assert!(!preview.to_string().contains(&f.personal.to_string()));
    j.request(
        "PATCH",
        &format!("/api/v1/platform/deployments/{deployment}"),
        Some(json!({"enabled":false})),
    )
    .await;
    // Re-enabling a disabled route gains a serving route.
    let gained = j
        .request(
            "POST",
            &format!("/api/v1/platform/deployments/{deployment}/configuration/preview"),
            Some(json!({"enabled":true})),
        )
        .await;
    assert_eq!(gained["serving_routes_gained"].as_array().unwrap().len(), 1);
    j.request(
        "PATCH",
        &format!("/api/v1/platform/deployments/{deployment}"),
        Some(json!({"enabled":true})),
    )
    .await;
    // The same change made with an admin token (catalog:write) records the token.
    let created = send(
        &j.client,
        &j.base,
        Some(&admin),
        "POST",
        "/api/v1/platform/admin-tokens",
        Some(json!({"name":"Contract token","scopes":["catalog:write"],"expires_in_days":1})),
    )
    .await;
    assert_eq!(created.status, StatusCode::OK);
    let token = serde_json::from_slice::<Value>(&created.bytes).unwrap()["token"]
        .as_str()
        .unwrap()
        .to_owned();
    let path = format!("/api/v1/platform/deployments/{deployment}");
    let response = j
        .client
        .patch(format!("{}{path}", j.base))
        .header("origin", ORIGIN)
        .bearer_auth(&token)
        .json(&json!({"enabled":false}))
        .send()
        .await
        .unwrap();
    let r = capture_http("PATCH", &path, response).await;
    assert_eq!(r.status, StatusCode::OK);
    j.observed.observe(&r);
    j.request("PATCH", &path, Some(json!({"enabled":true})))
        .await;
    // Earlier releases audited deployment changes without a snapshot: one with a
    // named actor, one without an actor (system), both values unavailable.
    let (legacy_user, legacy_system) = (Uuid::new_v4(), Uuid::new_v4());
    sqlx::query("INSERT INTO audit_events(id,actor_user_id,action,resource_type,resource_id,metadata,created_at) VALUES($1,$2,'configuration.updated','deployments',$3,'{}',now()-interval '1 day')").bind(legacy_user).bind(f.admin.user_id).bind(deployment).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO audit_events(id,action,resource_type,resource_id,metadata,created_at) VALUES($1,'deployment.created','deployment',$2,'{}',now()-interval '2 days')").bind(legacy_system).bind(deployment).execute(&pool).await.unwrap();
    // Telemetry and accounting of `contract/model` for the comparisons and policy windows.
    let (_, owner_key) = key(&f, &f.owner, f.team, Value::Null).await;
    let owner_key = id(&owner_key);
    seed_attempt(
        &pool,
        f.team,
        owner_key,
        anchor,
        "contract/model",
        "succeeded",
        Some(50),
        None,
    )
    .await;
    for path in [
        "/api/v1/platform/catalogs".into(),
        format!("/api/v1/platform/catalogs/{cat}"),
        format!("/api/v1/platform/catalogs/{cat}/models"),
        "/api/v1/platform/catalog-defaults".into(),
        "/api/v1/platform/models".into(),
        format!("/api/v1/platform/models/{model}"),
        format!("/api/v1/platform/models/{second}"),
        format!("/api/v1/platform/models/{model}/catalogs"),
        format!("/api/v1/platform/models/{model}/routing"),
        "/api/v1/platform/providers".into(),
        format!("/api/v1/platform/providers/{provider}"),
        format!("/api/v1/platform/providers/{bedrock_default}"),
        format!("/api/v1/platform/providers/{local_provider}"),
        "/api/v1/platform/deployments".into(),
        format!("/api/v1/platform/deployments/{deployment}"),
        format!("/api/v1/platform/deployments/{deployment}/prices"),
        format!("/api/v1/platform/deployments/{deployment}/routing"),
        format!("/api/v1/platform/deployments/{deployment}/batch-scheduling"),
        format!("/api/v1/platform/deployments/{deployment}/price-suggestion"),
        format!("/api/v1/platform/models/compare?ids={model},{second}"),
        format!("/api/v1/platform/models/compare?ids={model},{unpriced}"),
    ] {
        let value = j.request("GET", &path, None).await;
        assert!(!value.to_string().contains(&f.personal.to_string()));
    }
    // A route with no model price data nor a legacy-compatible protocol is unsupported.
    let detail = j
        .request(
            "GET",
            &format!("/api/v1/platform/models/{legacy_model}"),
            None,
        )
        .await;
    assert!(detail["readiness"]["unsupported_profiles"].is_array());
    // Optional-parameter coverage: every documented query parameter in a positive request.
    // The OpenRouter route's data policy is the configured one; the two picker
    // exclusions name workspaces/catalogs the model is not yet part of.
    let policy_now = crate::providers::openrouter::DataCollection::configured().as_str();
    let nested = j.request("GET", &format!("/api/v1/platform/models?limit=5&offset=0&q=contract&enabled=true&provider_connection_id={provider}&type=generation&max_input_price=9007199254740993&data_policy={policy_now}&include_deprecated=true&sort=name&not_in_catalog={}&not_direct_in={}", Uuid::new_v4(), Uuid::new_v4()), None).await;
    assert!(!nested.to_string().contains(&f.personal.to_string()));
    for sort in ["newest", "price"] {
        j.request("GET", &format!("/api/v1/platform/models?sort={sort}"), None)
            .await;
    }
    j.request(
        "GET",
        "/api/v1/platform/catalogs?limit=5&offset=0&q=contract",
        None,
    )
    .await;
    j.request(
        "GET",
        &format!("/api/v1/platform/catalogs/{cat}/models?limit=5&offset=0"),
        None,
    )
    .await;
    j.request("GET", &format!("/api/v1/platform/deployments?enabled=true&limit=5&offset=0&model_id={model}&provider_connection_id={provider}&q=contract"), None).await;
    j.request(
        "GET",
        "/api/v1/platform/providers?limit=10&offset=0&q=contract&enabled=false",
        None,
    )
    .await;
    j.request(
        "GET",
        &format!("/api/v1/platform/deployments/{deployment}/prices?limit=2&offset=1"),
        None,
    )
    .await;
    // The price suggestion has two real branches for tokens: priced from a concrete
    // endpoint, and from the catalog's top provider when no endpoint list exists;
    // and every other workload's units, unknown prices and missing ceilings.
    let catalog_only = id(&j.request("POST", "/api/v1/platform/deployments", Some(json!({"model_id":model,"provider_connection_id":provider,"upstream_model":"contract/catalog-only","enabled":true}))).await);
    let from_catalog = j
        .request(
            "GET",
            &format!("/api/v1/platform/deployments/{catalog_only}/price-suggestion"),
            None,
        )
        .await;
    assert_eq!(from_catalog["endpoint"]["selection"], "catalog");
    let from_endpoint = j
        .request(
            "GET",
            &format!("/api/v1/platform/deployments/{deployment}/price-suggestion"),
            None,
        )
        .await;
    assert_eq!(from_endpoint["endpoint"]["selection"], "cheapest");
    for route in &drafted {
        j.request(
            "GET",
            &format!("/api/v1/platform/deployments/{route}/price-suggestion"),
            None,
        )
        .await;
    }
    // PlatformRead is the declared boundary, but the handler needs platform_write.
    let r = send(
        &j.client,
        &j.base,
        Some(&auditor),
        "GET",
        &format!("/api/v1/platform/deployments/{deployment}/price-suggestion"),
        None,
    )
    .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    // An explicit endpoint null is an endpoint update: Bedrock clears it; any other kind is refused.
    j.request(
        "PATCH",
        &format!("/api/v1/platform/providers/{bedrock_default}"),
        Some(json!({"enabled":false,"endpoint":null})),
    )
    .await;
    let r = send(
        &j.client,
        &j.base,
        Some(&admin),
        "PATCH",
        &format!("/api/v1/platform/providers/{provider}"),
        Some(json!({"enabled":true,"endpoint":null})),
    )
    .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    // Configuration history: paged (cursor), every kind of entry in detail.
    let history = format!("/api/v1/platform/deployments/{deployment}/configuration/history");
    let first = j.request("GET", &format!("{history}?limit=2"), None).await;
    if let Some(cursor) = first["next_cursor"].as_str() {
        j.request("GET", &format!("{history}?limit=2&cursor={cursor}"), None)
            .await;
    }
    let all = j
        .request("GET", &format!("{history}?limit=100"), None)
        .await;
    let entries = all["data"].as_array().unwrap();
    for entry in entries {
        j.request("GET", &format!("{history}/{}", id(entry)), None)
            .await;
    }
    j.actor = &owner;
    j.request(
        "POST",
        &format!("/api/v1/workspaces/{}/models", f.team),
        Some(json!({"model_id":second})),
    )
    .await;
    // Policy layers, state C: the team's own (tighter) limits, then its accounting:
    // a settled request, and afterwards one whose cost is unknown.
    let policy = limits(
        10,
        1000,
        2,
        1,
        10_000,
        5,
        50,
        json!([{"period":"month","amount_microusd":"9007199254740993"},{"period":"lifetime","amount_microusd":"9007199254740993"}]),
    );
    j.request(
        "PUT",
        &format!("/api/v1/workspaces/{}/policy", f.team),
        Some(policy.clone()),
    )
    .await;
    for who in [&owner, &member] {
        j.actor = who;
        j.request("GET", &ws_policy, None).await;
    }
    seed_attempt(
        &pool,
        f.team,
        owner_key,
        anchor,
        "contract/model",
        "failed",
        None,
        Some(7),
    )
    .await;
    for who in [&owner, &member] {
        j.actor = who;
        j.request("GET", &ws_policy, None).await;
    }
    j.actor = &owner;
    for suffix in ["models", "available-models", "catalog", "policy"] {
        j.request(
            "GET",
            &format!("/api/v1/workspaces/{}/{suffix}", f.team),
            None,
        )
        .await;
    }
    for suffix in [
        "models?limit=5&offset=0&q=contract",
        "available-models?limit=5&offset=0&q=contract",
        "catalog?limit=5&offset=0&q=contract&type=generation&sort=price",
    ] {
        j.request(
            "GET",
            &format!("/api/v1/workspaces/{}/{suffix}", f.team),
            None,
        )
        .await;
    }
    j.request(
        "GET",
        &format!(
            "/api/v1/workspaces/{}/models/compare?ids={model},{second}",
            f.team
        ),
        None,
    )
    .await;
    // Live member redaction: limits visible, aggregate consumption hidden.
    j.actor = &member;
    let redacted = j
        .request(
            "GET",
            &format!("/api/v1/workspaces/{}/policy", f.team),
            None,
        )
        .await;
    assert_eq!(redacted["storage"]["usage_visible"], false);
    assert!(redacted["storage"]["used_bytes"].is_null());
    assert_eq!(redacted["budgets"][0]["headroom_status"], "restricted");
    assert!(redacted["budgets"][0]["used_microusd"].is_null());
    j.request(
        "GET",
        &format!(
            "/api/v1/workspaces/{}/models/compare?ids={model},{second}",
            f.team
        ),
        None,
    )
    .await;
    j.actor = &owner;
    j.request(
        "DELETE",
        &format!("/api/v1/workspaces/{}/models/{second}", f.team),
        None,
    )
    .await;
    // Catalog retirement cannot revoke independent direct assignments or
    // resurrect a retired catalog selection when availability returns.
    j.request(
        "POST",
        &format!("/api/v1/workspaces/{}/models", f.team),
        Some(json!({"model_id":second})),
    )
    .await;
    j.actor = &admin;
    j.request(
        "PUT",
        &format!("/api/v1/platform/catalogs/{cat}/models"),
        Some(json!({"model_ids":[]})),
    )
    .await;
    j.actor = &owner;
    let selected = j
        .request(
            "GET",
            &format!("/api/v1/workspaces/{}/models", f.team),
            None,
        )
        .await;
    assert_eq!(selected["data"].as_array().unwrap().len(), 1);
    assert_eq!(selected["data"][0]["direct_granted"], true);
    assert_eq!(selected["data"][0]["available_from_catalog"], false);
    let r = send(
        &j.client,
        &j.base,
        Some(&owner),
        "POST",
        &format!("/api/v1/workspaces/{}/models", f.team),
        Some(json!({"model_id":second})),
    )
    .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    j.actor = &admin;
    j.request(
        "PUT",
        &format!("/api/v1/platform/catalogs/{cat}/models"),
        Some(json!({"model_ids":[model,second]})),
    )
    .await;
    j.actor = &owner;
    let selected = j
        .request(
            "GET",
            &format!("/api/v1/workspaces/{}/models", f.team),
            None,
        )
        .await;
    assert_eq!(selected["data"].as_array().unwrap().len(), 1);
    j.actor = &admin;
    // Existing refusal assertions remain independent of contract validation.
    for actor in [&admin, &auditor] {
        let r = send(
            &j.client,
            &j.base,
            Some(actor),
            "GET",
            &format!("/api/v1/workspaces/{}/catalog", f.personal),
            None,
        )
        .await;
        assert_eq!(r.status, StatusCode::NOT_FOUND);
    }
    let r = send(
        &j.client,
        &j.base,
        Some(&auditor),
        "PATCH",
        &format!("/api/v1/platform/models/{model}"),
        Some(json!({"enabled":false})),
    )
    .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    let r = send(
        &j.client,
        &j.base,
        None,
        "GET",
        "/api/v1/platform/models",
        None,
    )
    .await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    let (status, inference_key) = key(&f, &f.owner, f.team, Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    let response = j
        .client
        .get(format!("{}/api/v1/platform/models", j.base))
        .bearer_auth(inference_key["token"].as_str().unwrap())
        .send()
        .await
        .unwrap();
    let r = capture_http("GET", "/api/v1/platform/models", response).await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    for (method, path) in [
        ("POST", "/api/v1/platform/providers".to_owned()),
        ("PUT", format!("/api/v1/workspaces/{}/policy", f.team)),
    ] {
        let valid = if method == "POST" {
            json!({"name":"Bad","provider":"openrouter","credential_ref":"env:CONTRACT_REFERENCE","enabled":true})
        } else {
            policy.clone()
        };
        let mut bad = valid;
        bad["unexpected_configuration"] = json!(true);
        let r = send(
            &j.client,
            &j.base,
            Some(if method == "POST" { &admin } else { &owner }),
            method,
            &path,
            Some(bad),
        )
        .await;
        assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
    }
    j.request(
        "DELETE",
        &format!("/api/v1/platform/deployments/{catalog_only}"),
        None,
    )
    .await;
    j.request(
        "DELETE",
        &format!("/api/v1/platform/deployments/{deployment}"),
        None,
    )
    .await;
    j.request(
        "DELETE",
        &format!("/api/v1/platform/providers/{disposable}"),
        None,
    )
    .await;
    j.request("DELETE", &format!("/api/v1/platform/models/{model}"), None)
        .await;
    j.request("DELETE", &format!("/api/v1/platform/catalogs/{cat}"), None)
        .await;
    j.observed.finish(SLICE3_AREAS);
}
