use super::*;

const SECRET_REF: &str = "env:SETUP_PRIVATE_REFERENCE";

async fn connection(pool: &PgPool, name: &str, enabled: bool) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO provider_connections(id,name,provider,credential_ref,enabled) VALUES($1,$2,'openai',$3,$4)")
        .bind(id)
        .bind(name)
        .bind(SECRET_REF)
        .bind(enabled)
        .execute(pool)
        .await
        .unwrap();
    id
}
async fn new_catalog(f: &Fixture, name: &str) -> Uuid {
    let (status, v) = call(
        &f.s,
        &f.admin,
        "POST",
        "/api/v1/platform/catalogs",
        json!({"name":name}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{v}");
    id(&v)
}
async fn route(pool: &PgPool, model: Uuid, connection: Uuid, enabled: bool, priced: bool) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO deployments(id,model_id,provider_connection_id,upstream_model,enabled) VALUES($1,$2,$3,'upstream',$4)").bind(id).bind(model).bind(connection).bind(enabled).execute(pool).await.unwrap();
    if priced {
        sqlx::query("INSERT INTO deployment_prices(id,deployment_id,input_microusd_per_million,output_microusd_per_million,input_token_limit,output_token_limit,pricing_version) VALUES($1,$2,1,1,10,10,1)").bind(Uuid::new_v4()).bind(id).execute(pool).await.unwrap();
    }
    id
}
fn price() -> Value {
    json!({"input_microusd_per_million":"1000000","output_microusd_per_million":"2000000","input_token_limit":100,"output_token_limit":50})
}
fn setup_body(name: &str, connection: Uuid, price: Value, catalogs: &[Uuid]) -> Value {
    json!({
        "model":{"public_name":name,"display_name":"Setup model","description":null,"supported_protocols":["chat_completions"],"enabled":false},
        "route":{"provider_connection_id":connection,"upstream_model":"upstream-model","enabled":false},
        "price":price,
        "catalog_ids":catalogs
    })
}
async fn counts(pool: &PgPool) -> Vec<i64> {
    let mut v = Vec::new();
    for table in [
        "models",
        "deployments",
        "deployment_prices",
        "catalog_models",
        "audit_events",
    ] {
        v.push(
            sqlx::query_scalar(&format!("SELECT count(*) FROM {table}"))
                .fetch_one(pool)
                .await
                .unwrap(),
        );
    }
    v
}
async fn get(f: &Fixture, u: &BrowserPrincipal, path: &str) -> (StatusCode, Value) {
    call(&f.s, u, "GET", path, json!({})).await
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn model_setup_is_one_audited_transaction(pool: PgPool) {
    let f = fixture(&pool).await;
    let conn = connection(&pool, "Cloud", true).await;
    let c1 = new_catalog(&f, "One").await;
    let c2 = new_catalog(&f, "Two").await;
    let (status, v) = call(
        &f.s,
        &f.admin,
        "POST",
        "/api/v1/platform/model-setup",
        setup_body("setup/model", conn, price(), &[c1, c2, c1]),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{v}");
    let model_id = Uuid::parse_str(v["model_id"].as_str().unwrap()).unwrap();
    let deployment_id = Uuid::parse_str(v["deployment_id"].as_str().unwrap()).unwrap();
    let price_id = Uuid::parse_str(v["price_id"].as_str().unwrap()).unwrap();
    let (m_status, m) = get(
        &f,
        &f.auditor,
        &format!("/api/v1/platform/models/{model_id}"),
    )
    .await;
    assert_eq!(m_status, StatusCode::OK);
    assert_eq!(m["public_name"], "setup/model");
    assert_eq!(m["enabled"], false);
    assert_eq!(
        m["readiness"],
        json!({"routes":1,"enabled_routes":0,"priced_enabled_routes":0,"serving_routes":0,"unsupported_routes":0,"unsupported_profiles":[],"type_tokens_per_minute":null,"routes_over_token_limit":0,"openrouter_free_routes":0,"catalogs":2,"direct_workspaces":0,"connections":[{"id":conn,"name":"Cloud"}]})
    );
    let deployment: (Uuid, Uuid, String, bool) = sqlx::query_as(
        "SELECT model_id,provider_connection_id,upstream_model,enabled FROM deployments WHERE id=$1",
    )
    .bind(deployment_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        deployment,
        (model_id, conn, "upstream-model".to_owned(), false)
    );
    let (p, prices) = get(
        &f,
        &f.auditor,
        &format!("/api/v1/platform/deployments/{deployment_id}/prices"),
    )
    .await;
    assert_eq!(p, StatusCode::OK, "{prices}");
    assert_eq!(prices["data"][0]["id"], price_id.to_string());
    assert_eq!(prices["data"][0]["input_microusd_per_million"], "1000000");
    let stored: (Uuid, i64, i16) = sqlx::query_as(
        "SELECT deployment_id,output_microusd_per_million,pricing_version FROM deployment_prices WHERE id=$1",
    )
    .bind(price_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(stored, (deployment_id, 2_000_000, 1));
    let (_, cats) = get(
        &f,
        &f.auditor,
        &format!("/api/v1/platform/models/{model_id}/catalogs"),
    )
    .await;
    let mut expected = vec![c1, c2];
    expected.sort();
    assert_eq!(cats, json!({"catalog_ids":expected}));
    let actions: Vec<(String, Option<Uuid>, Value)> = sqlx::query_as(
        "SELECT action,resource_id,metadata FROM audit_events WHERE action NOT IN ('catalog.created') ORDER BY action,resource_id",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    let mut catalogs_sorted = [c1, c2];
    catalogs_sorted.sort();
    assert_eq!(
        actions,
        vec![
            (
                "catalog.models_replaced".to_owned(),
                Some(catalogs_sorted[0]),
                json!({"count":1})
            ),
            (
                "catalog.models_replaced".to_owned(),
                Some(catalogs_sorted[1]),
                json!({"count":1})
            ),
            (
                "deployment.created".to_owned(),
                Some(deployment_id),
                json!({})
            ),
            ("model.created".to_owned(), Some(model_id), json!({})),
            ("price.created".to_owned(), Some(price_id), json!({})),
        ]
    );
    // Unpriced setup without catalogs returns a null price id.
    let (status, v) = call(
        &f.s,
        &f.admin,
        "POST",
        "/api/v1/platform/model-setup",
        setup_body("setup/unpriced", conn, Value::Null, &[]),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{v}");
    assert_eq!(v["price_id"], Value::Null);
    assert!(!v.to_string().contains("SETUP_PRIVATE_REFERENCE"));
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn model_setup_failure_at_any_stage_creates_nothing(pool: PgPool) {
    let f = fixture(&pool).await;
    let conn = connection(&pool, "Cloud", true).await;
    let c = new_catalog(&f, "Existing").await;
    let (status, _) = call(
        &f.s,
        &f.admin,
        "POST",
        "/api/v1/platform/model-setup",
        setup_body("taken/name", conn, Value::Null, &[]),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let before = counts(&pool).await;
    let mut bad_price = price();
    bad_price["input_microusd_per_million"] = json!("1.5");
    let mut bad_version = price();
    bad_version["pricing_version"] = json!(2);
    let mut bad_model = setup_body("bad model name", conn, Value::Null, &[]);
    bad_model["model"]["public_name"] = json!("spaces are invalid");
    let mut bad_route = setup_body("bad/route", conn, Value::Null, &[]);
    bad_route["route"]["upstream_model"] = json!(" ");
    let mut unknown_field = setup_body("bad/field", conn, Value::Null, &[]);
    unknown_field["route"]["model_id"] = json!(Uuid::new_v4());
    let too_many: Vec<Uuid> = (0..201).map(|_| Uuid::new_v4()).collect();
    for (case, body, expected) in [
        (
            "invalid price",
            setup_body("fresh/a", conn, bad_price, &[c]),
            StatusCode::BAD_REQUEST,
        ),
        (
            "invalid price version",
            setup_body("fresh/b", conn, bad_version, &[c]),
            StatusCode::BAD_REQUEST,
        ),
        (
            "unknown catalog",
            setup_body("fresh/c", conn, price(), &[c, Uuid::new_v4()]),
            StatusCode::BAD_REQUEST,
        ),
        (
            "too many catalogs",
            setup_body("fresh/d", conn, price(), &too_many),
            StatusCode::BAD_REQUEST,
        ),
        (
            "unknown provider connection",
            setup_body("fresh/e", Uuid::new_v4(), price(), &[c]),
            StatusCode::CONFLICT,
        ),
        (
            "duplicate public name",
            setup_body("taken/name", conn, price(), &[c]),
            StatusCode::CONFLICT,
        ),
        ("invalid model", bad_model, StatusCode::BAD_REQUEST),
        ("invalid route", bad_route, StatusCode::BAD_REQUEST),
    ] {
        let (status, v) = call(&f.s, &f.admin, "POST", "/api/v1/platform/model-setup", body).await;
        assert_eq!(status, expected, "{case}: {v}");
        assert_eq!(counts(&pool).await, before, "{case} persisted state");
    }
    let (status, _) = call(
        &f.s,
        &f.admin,
        "POST",
        "/api/v1/platform/model-setup",
        unknown_field,
    )
    .await;
    assert!(status.is_client_error());
    // Audit failure at the final stage still rolls back every earlier insert.
    sqlx::query("CREATE FUNCTION reject_catalog_audit() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.action='catalog.models_replaced' THEN RAISE EXCEPTION 'audit outage'; END IF; RETURN NEW; END $$").execute(&pool).await.unwrap();
    sqlx::query("CREATE TRIGGER reject_catalog_audit BEFORE INSERT ON audit_events FOR EACH ROW EXECUTE FUNCTION reject_catalog_audit()").execute(&pool).await.unwrap();
    let (status, _) = call(
        &f.s,
        &f.admin,
        "POST",
        "/api/v1/platform/model-setup",
        setup_body("fresh/f", conn, price(), &[c]),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(counts(&pool).await, before);
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn setup_overview_and_model_catalogs_are_admin_write_auditor_read_member_denied(
    pool: PgPool,
) {
    let f = fixture(&pool).await;
    let conn = connection(&pool, "Cloud", true).await;
    let c = new_catalog(&f, "Catalog").await;
    let m = model(&pool, "existing").await;
    let before = counts(&pool).await;
    for u in [&f.auditor, &f.member, &f.owner] {
        for (method, path, body) in [
            (
                "POST",
                "/api/v1/platform/model-setup".to_owned(),
                setup_body("denied/model", conn, price(), &[c]),
            ),
            (
                "PUT",
                format!("/api/v1/platform/models/{m}/catalogs"),
                json!({"catalog_ids":[c]}),
            ),
        ] {
            assert_eq!(
                call(&f.s, u, method, &path, body).await.0,
                StatusCode::FORBIDDEN,
                "{method} {path}"
            );
        }
    }
    assert_eq!(counts(&pool).await, before);
    for path in [
        "/api/v1/platform/overview".to_owned(),
        "/api/v1/platform/models".to_owned(),
        format!("/api/v1/platform/models/{m}"),
        format!("/api/v1/platform/models/{m}/catalogs"),
        format!("/api/v1/platform/models?provider_connection_id={conn}"),
        "/api/v1/platform/providers".to_owned(),
        format!("/api/v1/platform/providers/{conn}"),
    ] {
        for u in [&f.admin, &f.auditor] {
            let (status, body) = get(&f, u, &path).await;
            assert_eq!(status, StatusCode::OK, "{path}");
            assert!(!body.to_string().contains("SETUP_PRIVATE_REFERENCE"));
            assert!(!body.to_string().contains("@example.test"));
        }
        assert_eq!(
            get(&f, &f.member, &path).await.0,
            StatusCode::FORBIDDEN,
            "{path}"
        );
    }
    assert_eq!(
        get(
            &f,
            &f.admin,
            &format!("/api/v1/platform/models/{}/catalogs", Uuid::new_v4())
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        call(
            &f.s,
            &f.admin,
            "PUT",
            &format!("/api/v1/platform/models/{}/catalogs", Uuid::new_v4()),
            json!({"catalog_ids":[]})
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    for body in [
        json!({"catalog_ids":[Uuid::new_v4()]}),
        json!({"catalog_ids":(0..201).map(|_| Uuid::new_v4()).collect::<Vec<_>>()}),
    ] {
        assert_eq!(
            call(
                &f.s,
                &f.admin,
                "PUT",
                &format!("/api/v1/platform/models/{m}/catalogs"),
                body
            )
            .await
            .0,
            StatusCode::BAD_REQUEST
        );
    }
    assert_eq!(
        get(
            &f,
            &f.admin,
            "/api/v1/platform/providers?provider_connection_id=00000000-0000-0000-0000-000000000000"
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn readiness_counts_routes_connections_pricing_and_offer_sources(pool: PgPool) {
    let f = fixture(&pool).await;
    let on = connection(&pool, "Zulu", true).await;
    let off = connection(&pool, "Alpha", false).await;
    let unused = connection(&pool, "Unused", true).await;
    let catalog_model = model(&pool, "a/catalog").await;
    let direct_model = model(&pool, "b/direct").await;
    model(&pool, "c/bare").await;
    route(&pool, catalog_model, on, true, true).await; // enabled + priced
    route(&pool, catalog_model, on, true, false).await; // enabled, unpriced
    route(&pool, catalog_model, off, true, true).await; // disabled connection
    route(&pool, catalog_model, on, false, true).await; // disabled route
    route(&pool, direct_model, off, true, true).await;
    let c = new_catalog(&f, "Offer").await;
    assert_eq!(
        call(
            &f.s,
            &f.admin,
            "PUT",
            &format!("/api/v1/platform/models/{catalog_model}/catalogs"),
            json!({"catalog_ids":[c]})
        )
        .await
        .0,
        StatusCode::OK
    );
    direct(&pool, f.team, direct_model).await;
    direct(&pool, f.project, direct_model).await;
    let (status, list) = get(&f, &f.auditor, "/api/v1/platform/models").await;
    assert_eq!(status, StatusCode::OK, "{list}");
    let data = list["data"].as_array().unwrap();
    assert_eq!(
        data[0]["readiness"],
        json!({"routes":4,"enabled_routes":2,"priced_enabled_routes":1,"serving_routes":2,"unsupported_routes":0,"unsupported_profiles":[],"type_tokens_per_minute":null,"routes_over_token_limit":0,"openrouter_free_routes":0,"catalogs":1,"direct_workspaces":0,"connections":[{"id":off,"name":"Alpha"},{"id":on,"name":"Zulu"}]})
    );
    assert_eq!(
        data[1]["readiness"],
        json!({"routes":1,"enabled_routes":0,"priced_enabled_routes":0,"serving_routes":0,"unsupported_routes":0,"unsupported_profiles":[],"type_tokens_per_minute":null,"routes_over_token_limit":0,"openrouter_free_routes":0,"catalogs":0,"direct_workspaces":2,"connections":[{"id":off,"name":"Alpha"}]})
    );
    assert_eq!(
        data[2]["readiness"],
        json!({"routes":0,"enabled_routes":0,"priced_enabled_routes":0,"serving_routes":0,"unsupported_routes":0,"unsupported_profiles":[],"type_tokens_per_minute":null,"routes_over_token_limit":0,"openrouter_free_routes":0,"catalogs":0,"direct_workspaces":0,"connections":[]})
    );
    // Disabled workspaces no longer count as direct offers.
    sqlx::query("UPDATE workspaces SET disabled_at=now() WHERE id=$1")
        .bind(f.project)
        .execute(&pool)
        .await
        .unwrap();
    let (_, detail) = get(
        &f,
        &f.admin,
        &format!("/api/v1/platform/models/{direct_model}"),
    )
    .await;
    assert_eq!(detail["readiness"]["direct_workspaces"], 1);
    // Connection filter: models with any route on the connection, regardless of state.
    let (_, filtered) = get(
        &f,
        &f.admin,
        &format!("/api/v1/platform/models?provider_connection_id={off}"),
    )
    .await;
    let names: Vec<&str> = filtered["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["public_name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["a/catalog", "b/direct"]);
    let (_, filtered) = get(
        &f,
        &f.admin,
        &format!("/api/v1/platform/models?provider_connection_id={unused}"),
    )
    .await;
    assert_eq!(filtered["data"], json!([]));
    let (_, providers) = get(&f, &f.auditor, "/api/v1/platform/providers").await;
    let counts: Vec<(String, i64)> = providers["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| {
            (
                p["name"].as_str().unwrap().to_owned(),
                p["model_count"].as_i64().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        counts,
        vec![
            ("Alpha".to_owned(), 2),
            ("Unused".to_owned(), 0),
            ("Zulu".to_owned(), 1)
        ]
    );
    let (_, p) = get(&f, &f.auditor, &format!("/api/v1/platform/providers/{on}")).await;
    assert_eq!(p["model_count"], 1);
    assert!(p.get("credential_ref").is_none());
    // Overview mirrors readiness: only the enabled catalog model with an enabled route is ready.
    sqlx::query("UPDATE models SET enabled=false WHERE id<>$1")
        .bind(catalog_model)
        .execute(&pool)
        .await
        .unwrap();
    let (_, o) = get(&f, &f.auditor, "/api/v1/platform/overview").await;
    assert_eq!(o["setup"]["ready_models"], 1);
    assert_eq!(o["setup"]["enabled_routes"], 2);
    assert_eq!(o["setup"]["priced_enabled_routes"], 1);
    sqlx::query("UPDATE provider_connections SET enabled=false WHERE id=$1")
        .bind(on)
        .execute(&pool)
        .await
        .unwrap();
    let (_, o) = get(&f, &f.auditor, "/api/v1/platform/overview").await;
    assert_eq!(o["setup"]["ready_models"], 0);
    assert_eq!(o["setup"]["enabled_routes"], 0);
}

async fn allowed(pool: &PgPool, ws: Uuid, m: Uuid) -> bool {
    sqlx::query_scalar("SELECT workspace_model_allowed($1,$2)")
        .bind(ws)
        .bind(m)
        .fetch_one(pool)
        .await
        .unwrap()
}
async fn put_model_catalogs(f: &Fixture, m: Uuid, catalogs: &[Uuid]) {
    assert_eq!(
        call(
            &f.s,
            &f.admin,
            "PUT",
            &format!("/api/v1/platform/models/{m}/catalogs"),
            json!({"catalog_ids":catalogs})
        )
        .await
        .0,
        StatusCode::OK
    );
}
async fn selections(pool: &PgPool, lineage: Uuid) -> Vec<Uuid> {
    sqlx::query_scalar(
        "SELECT model_id FROM key_model_selections WHERE governance_key_id=$1 ORDER BY model_id",
    )
    .bind(lineage)
    .fetch_all(pool)
    .await
    .unwrap()
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn model_catalog_put_retires_like_catalog_put_and_never_resurrects(pool: PgPool) {
    let f = fixture(&pool).await;
    let m = model(&pool, "retire/me").await;
    let other = model(&pool, "retire/other").await;
    let c1 = new_catalog(&f, "One").await;
    let c2 = new_catalog(&f, "Two").await;
    assert_eq!(
        call(
            &f.s,
            &f.admin,
            "PUT",
            "/api/v1/platform/workspace-types/team/catalogs",
            json!({"catalog_ids":[c1,c2]})
        )
        .await
        .0,
        StatusCode::OK
    );
    // The other catalog's existing members must survive model-scoped replacement.
    assert_eq!(
        call(
            &f.s,
            &f.admin,
            "PUT",
            &format!("/api/v1/platform/catalogs/{c2}/models"),
            json!({"model_ids":[other]})
        )
        .await
        .0,
        StatusCode::OK
    );
    put_model_catalogs(&f, m, &[c1, c2]).await;
    let (_, c2_models) = get(
        &f,
        &f.auditor,
        &format!("/api/v1/platform/catalogs/{c2}/models"),
    )
    .await;
    assert_eq!(c2_models["data"].as_array().unwrap().len(), 2);
    for model in [m, other] {
        assert_eq!(
            call(
                &f.s,
                &f.owner,
                "POST",
                &format!("/api/v1/workspaces/{}/models", f.team),
                json!({"model_id":model})
            )
            .await
            .0,
            StatusCode::OK
        );
    }
    let (status, k) = key(&f, &f.owner, f.team, json!([m, other])).await;
    assert_eq!(status, StatusCode::OK, "{k}");
    let lineage = id(&k);
    // Losing one of two eligible catalogs keeps access.
    put_model_catalogs(&f, m, &[c2]).await;
    assert!(allowed(&pool, f.team, m).await);
    assert_eq!(selections(&pool, lineage).await.len(), 2);
    // Losing the last eligible catalog retires the grant and key selection only for this model.
    put_model_catalogs(&f, m, &[]).await;
    assert!(!allowed(&pool, f.team, m).await);
    assert!(allowed(&pool, f.team, other).await);
    assert_eq!(selections(&pool, lineage).await, vec![other]);
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM key_model_restrictions WHERE governance_key_id=$1"
        )
        .bind(lineage)
        .fetch_one(&pool)
        .await
        .unwrap(),
        1
    );
    // Re-adding catalog eligibility never resurrects the workspace grant or key selection.
    put_model_catalogs(&f, m, &[c1, c2]).await;
    assert!(!allowed(&pool, f.team, m).await);
    assert_eq!(selections(&pool, lineage).await, vec![other]);
    // Direct assignment still survives catalog loss (independent provenance).
    direct(&pool, f.team, m).await;
    put_model_catalogs(&f, m, &[]).await;
    assert!(allowed(&pool, f.team, m).await);
    // Audit: one event per changed catalog, none for unchanged memberships.
    let replaced: Vec<(Uuid, Value)> = sqlx::query_as("SELECT resource_id,metadata FROM audit_events WHERE action='catalog.models_replaced' ORDER BY created_at,resource_id").fetch_all(&pool).await.unwrap();
    // catalog PUT(c2) + setup [c1,c2] (2) + drop c1 (1) + drop c2 (1) + re-add both (2) + drop both (2)
    assert_eq!(replaced.len(), 9);
    assert_eq!(replaced[0], (c2, json!({"count":1})));
    let mut setup_pair = vec![replaced[1].0, replaced[2].0];
    setup_pair.sort();
    let mut expected = vec![c1, c2];
    expected.sort();
    assert_eq!(setup_pair, expected);
    assert_eq!(replaced[3], (c1, json!({"count":0})));
    assert_eq!(replaced[4], (c2, json!({"count":1})));
    put_model_catalogs(&f, m, &[]).await;
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM audit_events WHERE action='catalog.models_replaced'"
        )
        .fetch_one(&pool)
        .await
        .unwrap(),
        9,
        "no-op replacement records no per-catalog event"
    );
    let (_, ids) = get(
        &f,
        &f.auditor,
        &format!("/api/v1/platform/models/{m}/catalogs"),
    )
    .await;
    assert_eq!(ids, json!({"catalog_ids":[]}));
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn overview_counts_setup_and_aggregates_recent_attempts_and_known_cost(pool: PgPool) {
    let f = fixture(&pool).await;
    let (_, empty) = get(&f, &f.auditor, "/api/v1/platform/overview").await;
    assert_eq!(
        empty,
        json!({
            "setup":{"connections":0,"enabled_connections":0,"models":0,"ready_models":0,"enabled_routes":0,"priced_enabled_routes":0,"catalogs":0,"type_defaults":{"personal":0,"team":0,"project":0},"entitled_users":5,"oidc_mappings":0},
            "glance":{"entitled_users":5,"teams":1,"projects":1,"ready_models":0,"attempts_7d":"0","known_cost_7d_microusd":"0"}
        })
    );
    let on = connection(&pool, "On", true).await;
    connection(&pool, "Off", false).await;
    let m = model(&pool, "overview/model").await;
    let d = route(&pool, m, on, true, true).await;
    let c = new_catalog(&f, "C").await;
    new_catalog(&f, "D").await;
    put_model_catalogs(&f, m, &[c]).await;
    for (kind, ids) in [
        ("team", vec![c]),
        ("project", vec![c]),
        ("personal", vec![]),
    ] {
        assert_eq!(
            call(
                &f.s,
                &f.admin,
                "PUT",
                &format!("/api/v1/platform/workspace-types/{kind}/catalogs"),
                json!({"catalog_ids":ids})
            )
            .await
            .0,
            StatusCode::OK
        );
    }
    sqlx::query("INSERT INTO oidc_group_mappings(id,issuer,group_value,target_kind,platform_role,enabled) VALUES($1,'https://idp.test','g1','platform','user',true),($2,'https://idp.test','g2','platform','admin',false)").bind(Uuid::new_v4()).bind(Uuid::new_v4()).execute(&pool).await.unwrap();
    // A suspended user is no longer entitled.
    sqlx::query("UPDATE users SET disabled_at=now() WHERE id=$1")
        .bind(f.outsider.user_id)
        .execute(&pool)
        .await
        .unwrap();
    let (status, k) = key(&f, &f.owner, f.team, Value::Null).await;
    assert_eq!(status, StatusCode::OK, "{k}");
    let kid = id(&k);
    let (status, pk) = key(&f, &f.owner, f.personal, Value::Null).await;
    assert_eq!(status, StatusCode::OK, "{pk}");
    let personal_key = id(&pk);
    // (workspace, key, age, settled cost)
    let rows = [
        (f.team, kid, "1 hour", Some(1_500_i64)),
        (f.team, kid, "2 days", Some(9_007_199_254_740_993_i64)),
        (f.personal, personal_key, "3 days", Some(7)),
        (f.team, kid, "1 day", None), // pending: attempt, unknown cost
        (f.team, kid, "8 days", Some(1_000_000)), // outside the window
    ];
    for (i, (ws, key_id, age, cost)) in rows.into_iter().enumerate() {
        let e = Uuid::new_v4();
        sqlx::query(&format!("INSERT INTO inference_executions(id,workspace_id,api_key_id,deployment_id,public_model,provider,streamed,state,root_request_id,started_at) VALUES($1,$2,$3,$4,'overview/model','openai',false,'succeeded',$1,now()-interval '{age}')")).bind(e).bind(ws).bind(key_id).bind(d).execute(&pool).await.unwrap();
        if i == 3 {
            sqlx::query("INSERT INTO governance_reservations(execution_id,workspace_id,api_key_id,deployment_id,admitted_at,minute_start,month_start,lease_expires_at,state,held_microusd) SELECT $1,$2,$3,$4,e.started_at,now(),now(),now(),'pending',5 FROM inference_executions e WHERE e.id=$1").bind(e).bind(ws).bind(key_id).bind(d).execute(&pool).await.unwrap();
        } else if let Some(cost) = cost {
            sqlx::query("INSERT INTO governance_reservations(execution_id,workspace_id,api_key_id,deployment_id,admitted_at,minute_start,month_start,lease_expires_at,state,actual_microusd,input_tokens,output_tokens) SELECT $1,$2,$3,$4,e.started_at,now(),now(),now(),'settled',$5,1,1 FROM inference_executions e WHERE e.id=$1").bind(e).bind(ws).bind(key_id).bind(d).bind(cost).execute(&pool).await.unwrap();
        }
    }
    // A recent attempt without any reservation still counts as an attempt.
    let e = Uuid::new_v4();
    sqlx::query("INSERT INTO inference_executions(id,workspace_id,api_key_id,deployment_id,public_model,provider,streamed,state,root_request_id,attempt_number) VALUES($1,$2,$3,$4,'overview/model','openai',false,'failed',$1,2)").bind(e).bind(f.team).bind(kid).bind(d).execute(&pool).await.unwrap();
    for u in [&f.admin, &f.auditor] {
        let (status, o) = get(&f, u, "/api/v1/platform/overview").await;
        assert_eq!(status, StatusCode::OK, "{o}");
        assert_eq!(
            o,
            json!({
                "setup":{"connections":2,"enabled_connections":1,"models":1,"ready_models":1,"enabled_routes":1,"priced_enabled_routes":1,"catalogs":2,"type_defaults":{"personal":0,"team":1,"project":1},"entitled_users":4,"oidc_mappings":1},
                "glance":{"entitled_users":4,"teams":1,"projects":1,"ready_models":1,"attempts_7d":"5","known_cost_7d_microusd":"9007199254742500"}
            })
        );
        let text = o.to_string();
        assert!(!text.contains("SETUP_PRIVATE_REFERENCE") && !text.contains("example.test"));
    }
    assert_eq!(
        get(&f, &f.member, "/api/v1/platform/overview").await.0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        get(&f, &f.outsider, "/api/v1/platform/overview").await.0,
        StatusCode::FORBIDDEN
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn readiness_flags_ceilings_over_type_token_limits_and_blocked_free_routes(pool: PgPool) {
    let f = fixture(&pool).await;
    let on = connection(&pool, "Cloud", true).await;
    let m = model(&pool, "a/large").await;
    let d = route(&pool, m, on, true, false).await;
    sqlx::query("INSERT INTO deployment_prices(id,deployment_id,input_microusd_per_million,output_microusd_per_million,input_token_limit,output_token_limit,pricing_version) VALUES($1,$2,1,1,1048576,943717,1)").bind(Uuid::new_v4()).bind(d).execute(&pool).await.unwrap();
    async fn readiness_of(f: &Fixture, m: Uuid) -> Value {
        get(f, &f.auditor, &format!("/api/v1/platform/models/{m}"))
            .await
            .1["readiness"]
            .clone()
    }
    let readiness = |f| readiness_of(f, m);
    // No tokens-per-minute default: nothing to compare against.
    let r = readiness(&f).await;
    assert_eq!(
        (
            r["type_tokens_per_minute"].clone(),
            r["routes_over_token_limit"].clone()
        ),
        (Value::Null, json!(0))
    );
    sqlx::query("INSERT INTO workspace_type_policies(kind,tokens_per_minute) VALUES('team',100000),('personal',2000000) ON CONFLICT(kind) DO UPDATE SET tokens_per_minute=excluded.tokens_per_minute").execute(&pool).await.unwrap();
    let r = readiness(&f).await;
    assert_eq!(
        (
            r["type_tokens_per_minute"].clone(),
            r["routes_over_token_limit"].clone()
        ),
        (json!(100000), json!(1))
    );
    // Only types whose default catalogs offer the model apply once any does.
    let c = new_catalog(&f, "Personal defaults").await;
    sqlx::query("INSERT INTO catalog_models(catalog_id,model_id) VALUES($1,$2)")
        .bind(c)
        .bind(m)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO workspace_type_catalogs(kind,catalog_id) VALUES('personal',$1)")
        .bind(c)
        .execute(&pool)
        .await
        .unwrap();
    let r = readiness(&f).await;
    assert_eq!(
        (
            r["type_tokens_per_minute"].clone(),
            r["routes_over_token_limit"].clone()
        ),
        (json!(2000000), json!(0))
    );
    // A conservative later price clears the warning (latest version wins).
    sqlx::query("DELETE FROM workspace_type_catalogs")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO deployment_prices(id,deployment_id,input_microusd_per_million,output_microusd_per_million,input_token_limit,output_token_limit,pricing_version,created_at) VALUES($1,$2,1,1,8192,1024,1,clock_timestamp()+interval '1 second')").bind(Uuid::new_v4()).bind(d).execute(&pool).await.unwrap();
    assert_eq!(readiness(&f).await["routes_over_token_limit"], 0);
    // `:free` OpenRouter routes are counted for the data-collection check.
    let router = Uuid::new_v4();
    sqlx::query("INSERT INTO provider_connections(id,name,provider,credential_ref,endpoint,enabled) VALUES($1,'OpenRouter','openrouter','env:OPENROUTER_API_KEY','https://openrouter.ai/api/v1',true)").bind(router).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO deployments(id,model_id,provider_connection_id,upstream_model,enabled) VALUES($1,$2,$3,'nvidia/rerank:free',true),($4,$2,$3,'nvidia/rerank-paid',true)").bind(Uuid::new_v4()).bind(m).bind(router).bind(Uuid::new_v4()).execute(&pool).await.unwrap();
    assert_eq!(readiness(&f).await["openrouter_free_routes"], 1);
    // The server policy is readable by Admin and Auditor, without secrets.
    let (status, policy) = get(&f, &f.auditor, "/api/v1/platform/server-policy").await;
    assert_eq!(status, StatusCode::OK);
    assert!(matches!(
        policy["openrouter"]["data_collection"].as_str(),
        Some("deny" | "allow")
    ));
    assert_eq!(
        crate::management::resources::server_policy_json(
            crate::providers::openrouter::DataCollection::Deny
        ),
        json!({"openrouter":{"data_collection":"deny","free_models_available":false}})
    );
}

async fn local_connection(pool: &PgPool, name: &str, profile: &str) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO provider_connections(id,name,provider,credential_ref,endpoint,enabled) VALUES($1,$2,$3,'none','http://127.0.0.1:8000/v1',true)")
        .bind(id)
        .bind(name)
        .bind(profile)
        .execute(pool)
        .await
        .unwrap();
    id
}
fn workload_setup(name: &str, protocol: &str, connection: Uuid) -> Value {
    let mut body = setup_body(name, connection, Value::Null, &[]);
    body["model"]["supported_protocols"] = json!([protocol]);
    body
}
fn assert_unsupported(status: StatusCode, v: &Value, profile: &str, workload: &str) {
    assert_eq!(status, StatusCode::BAD_REQUEST, "{v}");
    assert_eq!(v["error"]["code"], "400", "{v}");
    // Deprecated duplicate, kept for one release.
    assert_eq!(v["error"]["reason"], "route_unsupported_capability", "{v}");
    let message = v["error"]["message"].as_str().unwrap();
    assert!(
        message.contains(profile) && message.contains(workload),
        "{message}"
    );
}

/// Routes are validated against the shared capability table (the one the
/// inference path uses for `unsupported_capability`) at model setup, route
/// creation, route enabling and model protocol changes.
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn routes_the_adapter_cannot_serve_are_rejected(pool: PgPool) {
    let f = fixture(&pool).await;
    let vllm = local_connection(&pool, "Spark embeddings", "vllm").await;
    let ollama = local_connection(&pool, "Ollama", "ollama").await;
    let before = counts(&pool).await;
    for (protocol, conn, profile, workload) in [
        ("systemone", vllm, "vllm", "System One"),
        ("rerank", ollama, "ollama", "Rerank"),
        ("responses", vllm, "vllm", "Text generation"),
        ("images", ollama, "ollama", "Image generation"),
    ] {
        let (status, v) = call(
            &f.s,
            &f.admin,
            "POST",
            "/api/v1/platform/model-setup",
            workload_setup(&format!("bad/{protocol}"), protocol, conn),
        )
        .await;
        assert_unsupported(status, &v, profile, workload);
        assert_eq!(counts(&pool).await, before, "{protocol} persisted state");
    }
    // What the profile serves is accepted, including a text model whose other
    // protocols another route serves.
    for (name, protocol, conn) in [
        ("ok/rerank", "rerank", vllm),
        ("ok/systemone", "systemone", ollama),
        ("ok/embeddings", "embeddings", vllm),
    ] {
        let (status, v) = call(
            &f.s,
            &f.admin,
            "POST",
            "/api/v1/platform/model-setup",
            workload_setup(name, protocol, conn),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{name}: {v}");
    }
    let mut text = setup_body("ok/text", vllm, Value::Null, &[]);
    text["model"]["supported_protocols"] = json!(["chat_completions", "responses"]);
    let (status, v) = call(&f.s, &f.admin, "POST", "/api/v1/platform/model-setup", text).await;
    assert_eq!(status, StatusCode::CREATED, "{v}");

    // POST /platform/deployments uses the same rule.
    let (status, v) = call(
        &f.s,
        &f.admin,
        "POST",
        "/api/v1/platform/models",
        json!({"public_name":"decider","display_name":"Decider","description":null,"enabled":true,"supported_protocols":["systemone"]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{v}");
    let decider = id(&v);
    let before = counts(&pool).await;
    let (status, v) = call(
        &f.s,
        &f.admin,
        "POST",
        "/api/v1/platform/deployments",
        json!({"model_id":decider,"provider_connection_id":vllm,"upstream_model":"cygnet","enabled":true}),
    )
    .await;
    assert_unsupported(status, &v, "vllm", "System One");
    assert_eq!(counts(&pool).await, before);
    let (status, v) = call(
        &f.s,
        &f.admin,
        "POST",
        "/api/v1/platform/deployments",
        json!({"model_id":decider,"provider_connection_id":ollama,"upstream_model":"nimble","enabled":true}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{v}");

    // A model's protocols cannot move away from what its enabled routes serve.
    let (status, v) = call(
        &f.s,
        &f.admin,
        "PATCH",
        &format!("/api/v1/platform/models/{decider}"),
        json!({"supported_protocols":["rerank"]}),
    )
    .await;
    assert_unsupported(status, &v, "ollama", "Rerank");
    let stored: Vec<String> =
        sqlx::query_scalar("SELECT supported_protocols FROM models WHERE id=$1")
            .bind(decider)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(stored, ["systemone"]);
}

/// Routes accepted before validation existed are not migrated: readiness says
/// they cannot serve, enabling one again is refused, disabling is allowed.
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn existing_unservable_routes_read_not_serving(pool: PgPool) {
    let f = fixture(&pool).await;
    let vllm = local_connection(&pool, "Spark embeddings", "vllm").await;
    let cloud = connection(&pool, "Cloud", true).await;
    let m = model(&pool, "legacy/systemone").await;
    sqlx::query(
        "UPDATE models SET supported_protocols=ARRAY['systemone'],enabled=true WHERE id=$1",
    )
    .bind(m)
    .execute(&pool)
    .await
    .unwrap();
    let bad = route(&pool, m, vllm, true, true).await;
    let parked = route(&pool, m, vllm, false, false).await;
    let c = new_catalog(&f, "Offer").await;
    put_model_catalogs(&f, m, &[c]).await;
    direct(&pool, f.team, m).await;
    let (status, v) = get(&f, &f.auditor, &format!("/api/v1/platform/models/{m}")).await;
    assert_eq!(status, StatusCode::OK, "{v}");
    let r = &v["readiness"];
    assert_eq!(r["enabled_routes"], 1, "{r}");
    assert_eq!(r["serving_routes"], 0, "{r}");
    assert_eq!(r["unsupported_routes"], 1, "{r}");
    assert_eq!(r["unsupported_profiles"], json!(["vllm"]), "{r}");
    let (_, o) = get(&f, &f.auditor, "/api/v1/platform/overview").await;
    assert_eq!(o["setup"]["ready_models"], 0, "{o}");
    // Workspace members see it as not serving (no route that can serve).
    let (status, catalog) = get(
        &f,
        &f.owner,
        &format!("/api/v1/workspaces/{}/catalog", f.team),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{catalog}");
    let row = catalog["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["model_id"] == m.to_string())
        .unwrap();
    assert_eq!(row["routes"], 0, "{row}");
    // Enabling another unservable route is refused; disabling the bad one works.
    let (status, v) = call(
        &f.s,
        &f.admin,
        "PATCH",
        &format!("/api/v1/platform/deployments/{parked}"),
        json!({"enabled":true}),
    )
    .await;
    assert_unsupported(status, &v, "vllm", "System One");
    let (status, v) = call(
        &f.s,
        &f.admin,
        "PATCH",
        &format!("/api/v1/platform/deployments/{bad}"),
        json!({"enabled":false}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{v}");
    // A servable route makes it ready again.
    let other = model(&pool, "fine/chat").await;
    let fine = route(&pool, other, cloud, false, true).await;
    let (status, v) = call(
        &f.s,
        &f.admin,
        "PATCH",
        &format!("/api/v1/platform/deployments/{fine}"),
        json!({"enabled":true}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{v}");
}

/// Model setup publishes through the same price rule: an embeddings v3 price
/// must state every meter an embeddings route can use, or nothing is created.
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn model_setup_refuses_v3_prices_with_omitted_meters(pool: PgPool) {
    let f = fixture(&pool).await;
    let vllm = local_connection(&pool, "Spark embeddings", "vllm").await;
    let line = |meter: &str, amount: &str, batch: u64, unit: &str| json!({"meter":meter,"microusd_per_batch":amount,"batch":batch,"unit_label":unit,"sku_label":"Line"});
    let na = |meter: &str| json!({"meter":meter,"not_applicable":true});
    let lines = |extra: Vec<Value>| {
        let mut l = vec![
            line("input_tokens", "10000", 1_000_000, "/M tokens"),
            na("output_tokens"),
            na("cache_read_tokens"),
            na("cache_write_tokens"),
            na("cache_write_5m_tokens"),
            na("cache_write_1h_tokens"),
            line("requests", "0", 1, "/request"),
        ];
        l.extend(extra);
        json!({"pricing_version":3,"input_token_limit":8192,"output_token_limit":0,"price_lines":l})
    };
    let before = counts(&pool).await;
    let mut body = workload_setup("spark/embed", "embeddings", vllm);
    body["price"] = lines(vec![]);
    let (status, v) = call(&f.s, &f.admin, "POST", "/api/v1/platform/model-setup", body).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{v}");
    assert_eq!(v["error"]["code"], "400", "{v}");
    assert_eq!(v["error"]["reason"], "price_meters_incomplete");
    assert_eq!(
        v["error"]["missing_meters"],
        json!([
            "output_images",
            "input_characters",
            "input_audio_seconds_ms",
            "output_audio_seconds_ms",
            "search_units"
        ])
    );
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("Embeddings")
    );
    assert_eq!(counts(&pool).await, before);
    let mut body = workload_setup("spark/embed", "embeddings", vllm);
    body["price"] = lines(vec![
        na("output_images"),
        na("input_characters"),
        na("input_audio_seconds_ms"),
        na("output_audio_seconds_ms"),
        json!({"meter":"search_units","unknown":true}),
    ]);
    let (status, v) = call(&f.s, &f.admin, "POST", "/api/v1/platform/model-setup", body).await;
    assert_eq!(status, StatusCode::CREATED, "{v}");
}
