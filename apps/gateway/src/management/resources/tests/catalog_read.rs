use super::*;
async fn catalog(f: &Fixture, name: &str, models: &[Uuid]) -> Uuid {
    let (status, v) = call(
        &f.s,
        &f.admin,
        "POST",
        "/api/v1/platform/catalogs",
        json!({"name":name}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{v}");
    let id = id(&v);
    assert_eq!(
        call(
            &f.s,
            &f.admin,
            "PUT",
            &format!("/api/v1/platform/catalogs/{id}/models"),
            json!({"model_ids":models})
        )
        .await
        .0,
        StatusCode::OK
    );
    id
}
async fn defaults(f: &Fixture, kind: &str, catalogs: &[Uuid]) {
    assert_eq!(
        call(
            &f.s,
            &f.admin,
            "PUT",
            &format!("/api/v1/platform/workspace-types/{kind}/catalogs"),
            json!({"catalog_ids":catalogs})
        )
        .await
        .0,
        StatusCode::OK
    );
}
async fn select(f: &Fixture, ws: Uuid, m: Uuid) -> StatusCode {
    call(
        &f.s,
        &f.owner,
        "POST",
        &format!("/api/v1/workspaces/{ws}/models"),
        json!({"model_id":m}),
    )
    .await
    .0
}
async fn allowed(pool: &PgPool, ws: Uuid, m: Uuid) -> bool {
    sqlx::query_scalar("SELECT workspace_model_allowed($1,$2)")
        .bind(ws)
        .bind(m)
        .fetch_one(pool)
        .await
        .unwrap()
}
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn live_multi_catalog_defaults_replace_override_and_no_selection_resurrection(pool: PgPool) {
    let f = fixture(&pool).await;
    let m1 = model(&pool, "local").await;
    let m2 = model(&pool, "cloud").await;
    let c1 = catalog(&f, "Local", &[m1]).await;
    let c2 = catalog(&f, "Cloud", &[m1, m2]).await;
    defaults(&f, "team", &[c1, c2]).await;
    // Presence in a catalog is availability, not workspace authorization.
    assert!(!allowed(&pool, f.team, m1).await);
    assert_eq!(select(&f, f.team, m1).await, StatusCode::OK);
    assert_eq!(select(&f, f.team, m2).await, StatusCode::OK);
    assert_eq!(select(&f, f.project, m1).await, StatusCode::FORBIDDEN);
    assert_eq!(select(&f, f.personal, m1).await, StatusCode::FORBIDDEN);
    let (status, k) = key(&f, &f.owner, f.team, json!([m1, m2])).await;
    assert_eq!(status, StatusCode::OK, "{k}");
    let lineage = id(&k);
    defaults(&f, "team", &[c1]).await;
    assert!(allowed(&pool, f.team, m1).await);
    assert!(!allowed(&pool, f.team, m2).await);
    let selections: Vec<Uuid> = sqlx::query_scalar(
        "SELECT model_id FROM key_model_selections WHERE governance_key_id=$1 ORDER BY model_id",
    )
    .bind(lineage)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(selections, vec![m1]);
    defaults(&f, "team", &[c1, c2]).await;
    assert!(!allowed(&pool, f.team, m2).await);
    assert_eq!(select(&f, f.team, m2).await, StatusCode::OK);
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM key_model_selections WHERE governance_key_id=$1 AND model_id=$2"
        )
        .bind(lineage)
        .bind(m2)
        .fetch_one(&pool)
        .await
        .unwrap(),
        0
    );
    assert_eq!(
        call(
            &f.s,
            &f.admin,
            "PUT",
            &format!("/api/v1/platform/workspaces/{}/catalogs", f.team),
            json!({"mode":"replace","catalog_ids":[]})
        )
        .await
        .0,
        StatusCode::OK
    );
    assert!(!allowed(&pool, f.team, m1).await);
    assert!(!allowed(&pool, f.team, m2).await);
    let (_, settings) = call(
        &f.s,
        &f.auditor,
        "GET",
        &format!("/api/v1/platform/workspaces/{}/catalogs", f.team),
        json!({}),
    )
    .await;
    assert_eq!(
        settings,
        json!({"mode":"replace","catalog_ids":[],"effective_catalog_ids":[]})
    );
    assert_eq!(
        call(
            &f.s,
            &f.admin,
            "DELETE",
            &format!("/api/v1/platform/workspaces/{}/catalogs", f.team),
            json!({})
        )
        .await
        .0,
        StatusCode::OK
    );
    assert!(!allowed(&pool, f.team, m1).await);
    assert_eq!(select(&f, f.team, m1).await, StatusCode::OK);
    let (_, keys) = call(
        &f.s,
        &f.owner,
        "GET",
        &format!("/api/v1/workspaces/{}/keys", f.team),
        json!({}),
    )
    .await;
    assert_eq!(keys["data"][0]["model_ids"], json!([]));
    // Header survives retirement: restriction is deny-all rather than inherit.
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
}
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn direct_source_survives_catalog_loss_and_catalog_source_survives_direct_revocation(
    pool: PgPool,
) {
    let f = fixture(&pool).await;
    let m = model(&pool, "approved").await;
    let c1 = catalog(&f, "One", &[m]).await;
    let c2 = catalog(&f, "Two", &[m]).await;
    defaults(&f, "team", &[c1, c2]).await;
    assert_eq!(select(&f, f.team, m).await, StatusCode::OK);
    assert_eq!(
        call(
            &f.s,
            &f.admin,
            "POST",
            &format!("/api/v1/platform/workspaces/{}/models", f.team),
            json!({"model_id":m})
        )
        .await
        .0,
        StatusCode::OK
    );
    let (status, k) = key(&f, &f.owner, f.team, json!([m])).await;
    assert_eq!(status, StatusCode::OK);
    let lineage = id(&k);
    assert_eq!(
        call(
            &f.s,
            &f.admin,
            "DELETE",
            &format!("/api/v1/platform/catalogs/{c1}"),
            json!({})
        )
        .await
        .0,
        StatusCode::OK
    );
    assert!(allowed(&pool, f.team, m).await);
    defaults(&f, "team", &[]).await;
    assert!(allowed(&pool, f.team, m).await);
    assert_eq!(
        call(
            &f.s,
            &f.owner,
            "DELETE",
            &format!("/api/v1/workspaces/{}/models/{m}", f.team),
            json!({})
        )
        .await
        .0,
        StatusCode::OK
    );
    assert!(allowed(&pool, f.team, m).await);
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM key_model_selections WHERE governance_key_id=$1"
        )
        .bind(lineage)
        .fetch_one(&pool)
        .await
        .unwrap(),
        1
    );
    defaults(&f, "team", &[c2]).await;
    assert_eq!(select(&f, f.team, m).await, StatusCode::OK);
    assert_eq!(
        call(
            &f.s,
            &f.admin,
            "DELETE",
            &format!("/api/v1/platform/workspaces/{}/models/{m}", f.team),
            json!({})
        )
        .await
        .0,
        StatusCode::OK
    );
    assert!(allowed(&pool, f.team, m).await);
    assert_eq!(
        call(
            &f.s,
            &f.member,
            "POST",
            &format!("/api/v1/workspaces/{}/models", f.team),
            json!({"model_id":m})
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        call(
            &f.s,
            &f.owner,
            "POST",
            &format!("/api/v1/platform/workspaces/{}/models", f.team),
            json!({"model_id":m})
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
}
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn self_service_directory_flags_literal_queries_and_personal_defaults(pool: PgPool) {
    let f = fixture(&pool).await;
    let m1 = model(&pool, "model_a").await;
    let m2 = model(&pool, "model%b").await;
    let m3 = model(&pool, "outside").await;
    let c = catalog(&f, "Personal", &[m1, m2]).await;
    defaults(&f, "personal", &[c]).await;
    direct(&pool, f.personal, m3).await;
    let (status, v) = call(
        &f.s,
        &f.owner,
        "GET",
        &format!("/api/v1/workspaces/{}/available-models?q=%25", f.personal),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["data"].as_array().unwrap().len(), 1);
    assert_eq!(v["data"][0]["model_id"], m2.to_string());
    assert_eq!(v["data"][0]["selected"], false);
    assert_eq!(v["data"][0]["available_from_catalog"], true);
    assert_eq!(select(&f, f.personal, m1).await, StatusCode::OK);
    let (_, v) = call(
        &f.s,
        &f.owner,
        "GET",
        &format!("/api/v1/workspaces/{}/models", f.personal),
        json!({}),
    )
    .await;
    assert_eq!(v["data"].as_array().unwrap().len(), 2);
    let outside = v["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["model_id"] == m3.to_string())
        .unwrap();
    assert_eq!(outside["direct_granted"], true);
    assert_eq!(outside["available_from_catalog"], false);
    assert_eq!(
        call(
            &f.s,
            &f.owner,
            "POST",
            &format!("/api/v1/workspaces/{}/models", f.personal),
            json!({"model_id":m3})
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
}
