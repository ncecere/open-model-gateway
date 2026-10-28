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
    let catalog_app = axum::Router::new().route("/models", axum::routing::get(|| async { Json(json!({"data":[{"id":"contract/model","context_length":8192,"top_provider":{"max_completion_tokens":1024},"pricing":{"prompt":"0.000000123","completion":"0.000000456"}}]})) }));
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
    let cat = id(&j
        .request(
            "POST",
            "/api/v1/platform/catalogs",
            Some(json!({"name":"Contract catalog","description":"Live defaults"})),
        )
        .await);
    let provider = id(&j.request("POST", "/api/v1/platform/providers", Some(json!({"name":"Contract reference","provider":"openrouter","credential_ref":"env:CONTRACT_REFERENCE","enabled":true}))).await);
    let model = id(&j.request("POST", "/api/v1/platform/models", Some(json!({"public_name":"contract/model","display_name":"Contract model","enabled":true}))).await);
    let deployment = id(&j.request("POST", "/api/v1/platform/deployments", Some(json!({"model_id":model,"provider_connection_id":provider,"upstream_model":"contract/model","enabled":true}))).await);
    j.request(
        "PATCH",
        &format!("/api/v1/platform/catalogs/{cat}"),
        Some(json!({"name":"Contract renamed"})),
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
        Some(json!({"model_ids":[model]})),
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
    // Both immutable legacy and v3 versions (money beyond JS's exact integer range).
    j.request(
        "POST",
        &format!("/api/v1/platform/deployments/{deployment}/prices"),
        Some(legacy_price()),
    )
    .await;
    j.request("POST", &format!("/api/v1/platform/deployments/{deployment}/prices"), Some(json!({"pricing_version":2,"input_microusd_per_million":"1","output_microusd_per_million":"2","input_token_limit":100,"output_token_limit":10,"cache_pricing":{"read":{"status":"priced","microusd_per_million":"1"},"write":{"status":"unknown"},"write_5m":{"status":"not_applicable"},"write_1h":{"status":"priced","microusd_per_million":"2"}}}))).await;
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
    j.request("PUT", &format!("/api/v1/platform/models/{model}/routing"), Some(json!({"strategy":"priority","max_attempts":2,"allow_ambiguous_failover":false,"required_residency":null}))).await;
    j.request("PUT", &format!("/api/v1/platform/deployments/{deployment}/routing"), Some(json!({"priority":-1,"weight":1,"residency":null,"failure_threshold":3,"cooldown_seconds":30}))).await;
    j.request("PUT", &format!("/api/v1/platform/deployments/{deployment}/batch-scheduling"), Some(json!({"max_concurrency":3,"yield_live_threshold":5,"window":{"timezone":"UTC","days":["mon","fri"],"start":"23:00","end":"06:00"}}))).await;
    let setup = j.request("POST", "/api/v1/platform/model-setup", Some(json!({"model":{"public_name":"contract/second","display_name":"Second model","enabled":true},"route":{"provider_connection_id":provider,"upstream_model":"contract/model","enabled":true},"price":v3_price(),"catalog_ids":[cat]}))).await;
    let second = Uuid::parse_str(setup["model_id"].as_str().unwrap()).unwrap();
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
    j.request(
        "PATCH",
        &format!("/api/v1/platform/deployments/{deployment}"),
        Some(json!({"enabled":true})),
    )
    .await;
    for path in [
        "/api/v1/platform/catalogs".into(),
        format!("/api/v1/platform/catalogs/{cat}"),
        format!("/api/v1/platform/catalogs/{cat}/models"),
        "/api/v1/platform/catalog-defaults".into(),
        "/api/v1/platform/models".into(),
        format!("/api/v1/platform/models/{model}"),
        format!("/api/v1/platform/models/{model}/catalogs"),
        format!("/api/v1/platform/models/{model}/routing"),
        "/api/v1/platform/providers".into(),
        format!("/api/v1/platform/providers/{provider}"),
        "/api/v1/platform/deployments".into(),
        format!("/api/v1/platform/deployments/{deployment}"),
        format!("/api/v1/platform/deployments/{deployment}/prices"),
        format!("/api/v1/platform/deployments/{deployment}/routing"),
        format!("/api/v1/platform/deployments/{deployment}/batch-scheduling"),
        format!("/api/v1/platform/deployments/{deployment}/price-suggestion"),
        format!("/api/v1/platform/models/compare?ids={model},{second}"),
    ] {
        let value = j.request("GET", &path, None).await;
        assert!(!value.to_string().contains(&f.personal.to_string()));
    }
    let history = j
        .request(
            "GET",
            &format!("/api/v1/platform/deployments/{deployment}/configuration/history?limit=1"),
            None,
        )
        .await;
    let change = id(&history["data"][0]);
    j.request(
        "GET",
        &format!("/api/v1/platform/deployments/{deployment}/configuration/history/{change}"),
        None,
    )
    .await;
    j.actor = &owner;
    j.request(
        "POST",
        &format!("/api/v1/workspaces/{}/models", f.team),
        Some(json!({"model_id":second})),
    )
    .await;
    let policy = json!({"requests_per_minute":10,"tokens_per_minute":1000,"concurrent_requests":2,"concurrent_jobs":1,"storage_bytes":10000,"key_issuances_per_holder":5,"key_issuances_per_workspace":50,"budgets":[{"period":"month","amount_microusd":"9007199254740993"}]});
    j.request(
        "PUT",
        &format!("/api/v1/workspaces/{}/policy", f.team),
        Some(policy.clone()),
    )
    .await;
    for suffix in ["models", "available-models", "catalog", "policy"] {
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
        &format!("/api/v1/platform/deployments/{deployment}"),
        None,
    )
    .await;
    j.request(
        "DELETE",
        &format!("/api/v1/platform/providers/{provider}"),
        None,
    )
    .await;
    j.request("DELETE", &format!("/api/v1/platform/models/{model}"), None)
        .await;
    j.request("DELETE", &format!("/api/v1/platform/catalogs/{cat}"), None)
        .await;
    j.observed.finish(SLICE3_AREAS);
}
