//! R06 (#66): Home's "Models you can use" from one bounded own-scope request
//! instead of one catalog request per workspace.
use super::*;

async fn serving_route(pool: &PgPool, model: Uuid) {
    let provider = Uuid::new_v4();
    sqlx::query("INSERT INTO provider_connections(id,name,provider,credential_ref,enabled) VALUES($1,$2,'openai','env:TEST',true)").bind(provider).bind(format!("Mock {provider}")).execute(pool).await.unwrap();
    sqlx::query("INSERT INTO deployments(id,model_id,provider_connection_id,upstream_model,enabled) VALUES($1,$2,$3,'Mock',true)").bind(Uuid::new_v4()).bind(model).bind(provider).execute(pool).await.unwrap();
}
fn names(v: &Value, key: &str) -> Vec<String> {
    v[key]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["public_name"].as_str().unwrap().to_owned())
        .collect()
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn me_models_aggregates_own_workspaces_only_and_stays_bounded(pool: PgPool) {
    let f = fixture(&pool).await;
    let smart = model(&pool, "company/smart").await;
    let dark = model(&pool, "company/dark").await;
    let foreign = model(&pool, "company/foreign").await;
    serving_route(&pool, smart).await;
    serving_route(&pool, foreign).await;
    direct(&pool, f.personal, smart).await;
    direct(&pool, f.team, smart).await;
    direct(&pool, f.personal, dark).await;
    // Another person's personal workspace never contributes.
    let outsider_personal = workspace(&pool, "Personal", "personal", f.outsider.user_id).await;
    direct(&pool, outsider_personal, foreign).await;

    let (status, owner) = call(&f.s, &f.owner, "GET", "/api/v1/me/models", Value::Null).await;
    assert_eq!(status, StatusCode::OK, "{owner}");
    assert_eq!(names(&owner, "models"), ["company/smart"]);
    let smart_row = &owner["models"][0];
    assert_eq!(smart_row["workspace_count"], 2);
    let ws: Vec<&str> = smart_row["workspaces"]
        .as_array()
        .unwrap()
        .iter()
        .map(|w| w["id"].as_str().unwrap())
        .collect();
    assert_eq!(
        ws,
        [f.personal.to_string(), f.team.to_string()],
        "personal first"
    );
    assert_eq!(owner["usable_count"], 1);
    assert_eq!(names(&owner, "not_serving"), ["company/dark"]);
    assert_eq!(owner["not_serving_count"], 1);
    assert_eq!(owner["workspaces_without_models"], json!([f.project]));

    // A member sees only what their own memberships give them.
    let (_, member) = call(&f.s, &f.member, "GET", "/api/v1/me/models", Value::Null).await;
    assert_eq!(names(&member, "models"), ["company/smart"]);
    assert_eq!(member["models"][0]["workspace_count"], 1);
    assert_eq!(member["not_serving"], json!([]));

    // Many workspaces: the response stays the same size; counts stay exact.
    for i in 0..40 {
        let extra = workspace(&pool, &format!("Extra {i:02}"), "team", f.owner.user_id).await;
        direct(&pool, extra, smart).await;
    }
    let (_, many) = call(
        &f.s,
        &f.owner,
        "GET",
        "/api/v1/me/models?limit=1",
        Value::Null,
    )
    .await;
    assert_eq!(many["models"].as_array().unwrap().len(), 1);
    assert_eq!(many["models"][0]["workspace_count"], 42);
    assert!(many["models"][0]["workspaces"].as_array().unwrap().len() <= 2);
    for bad in ["?limit=0", "?limit=51", "?limit=x"] {
        let (status, _) = call(
            &f.s,
            &f.owner,
            "GET",
            &format!("/api/v1/me/models{bad}"),
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}");
    }
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn me_summary_totals_only_skips_per_workspace_detail(pool: PgPool) {
    let f = fixture(&pool).await;
    let (status, full) = call(&f.s, &f.owner, "GET", "/api/v1/me/summary", Value::Null).await;
    assert_eq!(status, StatusCode::OK, "{full}");
    assert_eq!(full["workspaces"].as_array().unwrap().len(), 3);
    let (status, totals) = call(
        &f.s,
        &f.owner,
        "GET",
        "/api/v1/me/summary?totals_only=true",
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{totals}");
    assert_eq!(totals["totals"], full["totals"]);
    assert_eq!(totals["workspaces"], json!([]));
}
