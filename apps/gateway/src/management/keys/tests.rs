use super::*;
use crate::{
    auth::Principal,
    governance,
    inference::{
        error::InferenceError,
        repository::{ExecutionStart, InferenceRepository},
        types::{ChatRequest, Deployment},
    },
    management::tests::{add_member, call, fixture},
};
use sqlx::PgPool;

fn base(ws: Uuid) -> String {
    format!("/api/v1/workspaces/{ws}/keys")
}
async fn create(s: &Store, u: &BrowserPrincipal, ws: Uuid, ids: Value) -> Value {
    let (status, body) = call(
        s,
        u,
        "POST",
        &base(ws),
        json!({
            "name":"Restricted", "expires_in_days":30, "model_ids":ids
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body
}
fn id(body: &Value) -> Uuid {
    body["id"].as_str().unwrap().parse().unwrap()
}
async fn principal(s: &Store, body: &Value) -> Principal {
    s.authenticate(body["token"].as_str().unwrap())
        .await
        .unwrap()
        .unwrap()
}
async fn enabled(pool: &PgPool) -> (Uuid, Deployment) {
    sqlx::query("UPDATE deployments SET enabled=true")
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("UPDATE provider_connections SET enabled=true")
        .execute(pool)
        .await
        .unwrap();
    let m = sqlx::query_scalar("SELECT model_id FROM deployments LIMIT 1")
        .fetch_one(pool)
        .await
        .unwrap();
    let d = sqlx::query_as("SELECT d.id,p.provider,d.upstream_model,p.credential_ref,p.endpoint,p.region FROM deployments d JOIN provider_connections p ON p.id=d.provider_connection_id LIMIT 1").fetch_one(pool).await.unwrap();
    (m, d)
}
async fn model(pool: &PgPool, org: Uuid, ws: Option<Uuid>) -> Uuid {
    let m = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO models(id,public_name,display_name,enabled) VALUES($1,$2,'Test',true)",
    )
    .bind(m)
    .bind(m.to_string())
    .execute(pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO organization_model_grants(organization_id,model_id,public_name) VALUES($1,$2,$3)").bind(org).bind(m).bind(m.to_string()).execute(pool).await.unwrap();
    if let Some(ws) = ws {
        sqlx::query("INSERT INTO workspace_model_grants(organization_id,workspace_id,model_id) VALUES($1,$2,$3)").bind(org).bind(ws).bind(m).execute(pool).await.unwrap();
    }
    m
}
fn request() -> ChatRequest {
    ChatRequest {
        model: "company/smart".into(),
        messages: vec![],
        tools: vec![],
        tool_choice: None,
        temperature: None,
        max_output_tokens: Some(10),
        stream: false,
    }
}
fn record(p: Principal, d: &Deployment) -> ExecutionStart {
    let id = Uuid::new_v4();
    ExecutionStart {
        id,
        root_request_id: id,
        attempt_number: 1,
        principal: p,
        deployment_id: d.id,
        provider: d.provider.clone(),
        model: "company/smart".into(),
        streamed: false,
    }
}
async fn assert_access(s: &Store, p: Principal, d: &Deployment, allowed: bool) {
    assert_eq!(!s.visible_models(&p).await.unwrap().is_empty(), allowed);
    assert_model_access(s, p, d, "company/smart", allowed).await;
}
async fn assert_model_access(s: &Store, p: Principal, d: &Deployment, name: &str, allowed: bool) {
    assert_eq!(
        s.visible_models(&p)
            .await
            .unwrap()
            .iter()
            .any(|m| m.id == name),
        allowed
    );
    assert_eq!(!s.deployments(&p, name).await.unwrap().is_empty(), allowed);
    let mut record = record(p, d);
    record.model = name.into();
    let mut request = request();
    request.model = name.into();
    let result = governance::admit_for_deployment(s, &record, &request, 30, d).await;
    if allowed {
        result.unwrap();
    } else {
        assert_eq!(result, Err(InferenceError::ModelUnavailable));
        for (table, column) in [
            ("inference_executions", "id"),
            ("governance_reservations", "execution_id"),
            ("monetary_ledger", "execution_id"),
        ] {
            let count: i64 =
                sqlx::query_scalar(&format!("SELECT count(*) FROM {table} WHERE {column}=$1"))
                    .bind(record.id)
                    .fetch_one(&s.pool)
                    .await
                    .unwrap();
            assert_eq!(count, 0);
        }
    }
}
async fn counts(pool: &PgPool) -> (i64, i64, i64, i64) {
    sqlx::query_as("SELECT (SELECT count(*) FROM api_keys),(SELECT count(*) FROM audit_events),(SELECT count(*) FROM key_model_restrictions),(SELECT count(*) FROM key_model_selections)").fetch_one(pool).await.unwrap()
}

#[sqlx::test]
async fn omitted_null_empty_subset_and_legacy_agree_across_list_discovery_admission(pool: PgPool) {
    let (s, k, u, _) = fixture(&pool).await;
    let (m, d) = enabled(&pool).await;
    let legacy = s
        .authenticate(&k.personal_key.token)
        .await
        .unwrap()
        .unwrap();
    assert_access(&s, legacy, &d, true).await;
    let (status, omitted) = call(
        &s,
        &u,
        "POST",
        &base(k.personal_workspace_id),
        json!({"name":"Omitted","expires_in_days":30}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let null = create(&s, &u, k.personal_workspace_id, Value::Null).await;
    let empty = create(&s, &u, k.personal_workspace_id, json!([])).await;
    let subset = create(&s, &u, k.personal_workspace_id, json!([m, m])).await;
    for (body, expected, allowed) in [
        (&omitted, Value::Null, true),
        (&null, Value::Null, true),
        (&empty, json!([]), false),
        (&subset, json!([m]), true),
    ] {
        assert_eq!(body["model_ids"], expected);
        assert_access(&s, principal(&s, body).await, &d, allowed).await;
    }
    let listed = call(&s, &u, "GET", &base(k.personal_workspace_id), json!({}))
        .await
        .1;
    for body in [&omitted, &null, &empty, &subset] {
        let row = listed["data"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["id"] == body["id"])
            .unwrap();
        assert_eq!(row["model_ids"], body["model_ids"]);
        assert!(row.get("token").is_none());
    }
    let legacy_row = listed["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] == json!(legacy.key_id))
        .unwrap();
    assert_eq!(legacy_row["model_ids"], Value::Null);
}

#[sqlx::test]
async fn selection_validation_is_bounded_sorted_tenant_scoped_and_atomic(pool: PgPool) {
    let (s, k, u, _) = fixture(&pool).await;
    let (m, _) = enabled(&pool).await;
    let sibling = model(&pool, k.organization_id, Some(k.team_workspace_id)).await;
    let assigned_only = model(&pool, k.organization_id, None).await;
    let foreign_org = Uuid::new_v4();
    sqlx::query("INSERT INTO organizations(id,slug,name) VALUES($1,$2,'Foreign')")
        .bind(foreign_org)
        .bind(foreign_org.to_string())
        .execute(&pool)
        .await
        .unwrap();
    let foreign = model(&pool, foreign_org, None).await;
    let many: Vec<Uuid> = (0..201).map(|_| Uuid::new_v4()).collect();
    for ids in [
        json!(many),
        json!(vec![m; 201]),
        json!([Uuid::new_v4()]),
        json!([foreign]),
        json!([sibling]),
        json!([assigned_only]),
        json!([m, foreign]),
        json!(["company/smart"]),
    ] {
        let before = counts(&pool).await;
        let (status, _) = call(
            &s,
            &u,
            "POST",
            &base(k.personal_workspace_id),
            json!({"name":"Bad","expires_in_days":30,"model_ids":ids}),
        )
        .await;
        assert!(status.is_client_error(), "{status}");
        assert_eq!(counts(&pool).await, before);
    }
    let second = model(&pool, k.organization_id, Some(k.personal_workspace_id)).await;
    sqlx::query("UPDATE models SET enabled=false WHERE id=$1")
        .bind(second)
        .execute(&pool)
        .await
        .unwrap();
    let mut ordered = vec![m, second];
    ordered.sort_unstable();
    let body = create(
        &s,
        &u,
        k.personal_workspace_id,
        json!([second, m, second, m]),
    )
    .await;
    assert_eq!(body["model_ids"], json!(ordered));
    // A disabled granted model is preconfigurable, but unavailable for inference.
    let disabled = create(&s, &u, k.personal_workspace_id, json!([second])).await;
    let p = principal(&s, &disabled).await;
    assert!(s.visible_models(&p).await.unwrap().is_empty());
    assert!(
        s.deployments(&p, &second.to_string())
            .await
            .unwrap()
            .is_empty()
    );
    // Raw bound accepts 200 duplicate entries but still stores one selection.
    let body = create(&s, &u, k.personal_workspace_id, json!(vec![m; 200])).await;
    assert_eq!(body["model_ids"], json!([m]));
}

#[sqlx::test]
async fn personal_individual_grants_never_authorize_shared_or_service_keys(pool: PgPool) {
    let (s, k, u, other) = fixture(&pool).await;
    let (m, d) = enabled(&pool).await;
    sqlx::query("DELETE FROM workspace_model_grants WHERE organization_id=$1")
        .bind(k.organization_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO user_model_grants(organization_id,user_id,model_id) VALUES($1,$2,$3)")
        .bind(k.organization_id)
        .bind(u.user_id)
        .bind(m)
        .execute(&pool)
        .await
        .unwrap();
    let body = create(&s, &u, k.personal_workspace_id, json!([m])).await;
    assert_access(&s, principal(&s, &body).await, &d, true).await;
    let (status, sa) = call(
        &s,
        &u,
        "POST",
        &format!(
            "/api/v1/workspaces/{}/service-accounts",
            k.team_workspace_id
        ),
        json!({"name":"Service"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    for service in [Value::Null, sa["id"].clone()] {
        let before = counts(&pool).await;
        assert_eq!(call(&s,&u,"POST",&base(k.team_workspace_id),json!({"name":"No individual grants","expires_in_days":30,"model_ids":[m],"service_account_id":service})).await.0,StatusCode::BAD_REQUEST);
        assert_eq!(counts(&pool).await, before);
        let (status, b) = call(
            &s,
            &u,
            "POST",
            &base(k.team_workspace_id),
            json!({"name":"Inherit","expires_in_days":30,"service_account_id":service}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_access(&s, principal(&s, &b).await, &d, false).await;
    }
    sqlx::query("INSERT INTO workspace_model_grants(organization_id,workspace_id,model_id) VALUES($1,$2,$3)").bind(k.organization_id).bind(k.team_workspace_id).bind(m).execute(&pool).await.unwrap();
    let (status,service) = call(&s,&u,"POST",&base(k.team_workspace_id),json!({"name":"Service","expires_in_days":30,"model_ids":[m],"service_account_id":sa["id"]})).await;
    assert_eq!(status, StatusCode::OK);
    assert_access(&s, principal(&s, &service).await, &d, true).await;
    // Inherited organization administration isn't direct human-key membership.
    sqlx::query(
        "INSERT INTO organization_memberships(organization_id,user_id,role) VALUES($1,$2,'admin')",
    )
    .bind(k.organization_id)
    .bind(other.user_id)
    .execute(&pool)
    .await
    .unwrap();
    let before = counts(&pool).await;
    assert_eq!(
        call(
            &s,
            &other,
            "POST",
            &base(k.team_workspace_id),
            json!({"name":"Not a member","expires_in_days":30,"model_ids":[m]})
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(counts(&pool).await, before);
    // Workspace membership alone is also insufficient for human issuance.
    sqlx::query("UPDATE organization_memberships SET disabled_at=now() WHERE organization_id=$1 AND user_id=$2").bind(k.organization_id).bind(u.user_id).execute(&pool).await.unwrap();
    assert!(
        call(
            &s,
            &u,
            "POST",
            &base(k.team_workspace_id),
            json!({"name":"Disabled member","expires_in_days":30,"model_ids":[m]})
        )
        .await
        .0
        .is_client_error()
    );
}

#[sqlx::test]
async fn rotation_retains_lineage_selections_and_rejects_permission_edits(pool: PgPool) {
    let (s, k, u, _) = fixture(&pool).await;
    let (m, d) = enabled(&pool).await;
    for selections in [Value::Null, json!([]), json!([m])] {
        let mut body = create(&s, &u, k.personal_workspace_id, selections.clone()).await;
        let root = id(&body);
        for _ in 0..3 {
            let p = principal(&s, &body).await;
            let path = format!("{}/{}/rotate", base(k.personal_workspace_id), id(&body));
            let before = counts(&pool).await;
            assert!(
                call(
                    &s,
                    &u,
                    "POST",
                    &path,
                    json!({"expires_in_days":30,"model_ids":null})
                )
                .await
                .0
                .is_client_error()
            );
            assert_eq!(counts(&pool).await, before);
            assert_eq!(
                call(
                    &s,
                    &u,
                    "PATCH",
                    &format!("{}/{}", base(k.personal_workspace_id), root),
                    json!({"model_ids":null})
                )
                .await
                .0,
                StatusCode::METHOD_NOT_ALLOWED
            );
            let (status, next) = call(&s, &u, "POST", &path, json!({"expires_in_days":30})).await;
            assert_eq!(status, StatusCode::OK);
            assert!(
                s.authenticate(body["token"].as_str().unwrap())
                    .await
                    .unwrap()
                    .is_none()
            );
            assert_access(&s, p, &d, false).await;
            body = next;
            let lineage: Uuid =
                sqlx::query_scalar("SELECT governance_key_id FROM api_keys WHERE id=$1")
                    .bind(id(&body))
                    .fetch_one(&pool)
                    .await
                    .unwrap();
            assert_eq!(lineage, root);
            let listed = call(&s, &u, "GET", &base(k.personal_workspace_id), json!({}))
                .await
                .1;
            assert_eq!(
                listed["data"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|row| row["id"] == body["id"])
                    .unwrap()["model_ids"],
                selections
            );
            assert_access(&s, principal(&s, &body).await, &d, selections != json!([])).await;
        }
    }
}

#[sqlx::test]
async fn composite_foreign_keys_reject_cross_tenant_and_cross_workspace_links(pool: PgPool) {
    let (s, k, u, _) = fixture(&pool).await;
    let (m, _) = enabled(&pool).await;
    let body = create(&s, &u, k.personal_workspace_id, json!([m])).await;
    let foreign_org = Uuid::new_v4();
    sqlx::query("INSERT INTO organizations(id,slug,name) VALUES($1,$2,'Foreign')")
        .bind(foreign_org)
        .bind(foreign_org.to_string())
        .execute(&pool)
        .await
        .unwrap();
    let foreign_model = model(&pool, foreign_org, None).await;
    for (org, ws) in [
        (foreign_org, k.personal_workspace_id),
        (k.organization_id, k.team_workspace_id),
    ] {
        let err = sqlx::query("INSERT INTO key_model_restrictions(organization_id,workspace_id,governance_key_id) VALUES($1,$2,$3)").bind(org).bind(ws).bind(id(&body)).execute(&pool).await.unwrap_err();
        assert_eq!(
            err.as_database_error().unwrap().code().as_deref(),
            Some("23503")
        );
        let err = sqlx::query("INSERT INTO key_model_selections(organization_id,workspace_id,governance_key_id,model_id) VALUES($1,$2,$3,$4)").bind(org).bind(ws).bind(id(&body)).bind(m).execute(&pool).await.unwrap_err();
        assert_eq!(
            err.as_database_error().unwrap().code().as_deref(),
            Some("23503")
        );
    }
    let err = sqlx::query("INSERT INTO key_model_selections(organization_id,workspace_id,governance_key_id,model_id) VALUES($1,$2,$3,$4)").bind(k.organization_id).bind(k.personal_workspace_id).bind(id(&body)).bind(foreign_model).execute(&pool).await.unwrap_err();
    assert_eq!(
        err.as_database_error().unwrap().code().as_deref(),
        Some("23503")
    );
}

#[sqlx::test]
async fn parent_revocation_retains_header_and_regrant_cannot_revive_cached_target(pool: PgPool) {
    let (s, k, u, _) = fixture(&pool).await;
    let (m, d) = enabled(&pool).await;
    let body = create(&s, &u, k.personal_workspace_id, json!([m])).await;
    let p = principal(&s, &body).await;
    assert_access(&s, p, &d, true).await;
    sqlx::query("UPDATE users SET platform_admin=true WHERE id=$1")
        .bind(u.user_id)
        .execute(&pool)
        .await
        .unwrap();
    let path = format!("/api/v1/platform/orgs/{}/models/{m}", k.organization_id);
    assert_eq!(
        call(&s, &u, "DELETE", &path, json!({})).await.0,
        StatusCode::OK
    );
    assert_access(&s, p, &d, false).await;
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM key_model_restrictions WHERE governance_key_id=$1"
        )
        .bind(p.key_id)
        .fetch_one(&pool)
        .await
        .unwrap(),
        1
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM key_model_selections WHERE governance_key_id=$1"
        )
        .bind(p.key_id)
        .fetch_one(&pool)
        .await
        .unwrap(),
        0
    );
    assert_eq!(
        call(&s, &u, "PUT", &path, json!({"public_name":"company/smart"}))
            .await
            .0,
        StatusCode::OK
    );
    sqlx::query("INSERT INTO workspace_model_grants(organization_id,workspace_id,model_id) VALUES($1,$2,$3)").bind(k.organization_id).bind(k.personal_workspace_id).bind(m).execute(&pool).await.unwrap();
    assert_access(&s, p, &d, false).await;
    let legacy = s
        .authenticate(&k.personal_key.token)
        .await
        .unwrap()
        .unwrap();
    assert_access(&s, legacy, &d, true).await;
    let listed = call(&s, &u, "GET", &base(k.personal_workspace_id), json!({}))
        .await
        .1;
    assert_eq!(
        listed["data"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["id"] == body["id"])
            .unwrap()["model_ids"],
        json!([])
    );
}

#[sqlx::test]
async fn nonempty_allowlist_denies_another_enabled_granted_model(pool: PgPool) {
    let (s, k, u, _) = fixture(&pool).await;
    let (m, d) = enabled(&pool).await;
    let second = model(&pool, k.organization_id, Some(k.personal_workspace_id)).await;
    let deployment = Uuid::new_v4();
    sqlx::query("INSERT INTO deployments(id,model_id,provider_connection_id,upstream_model,enabled) SELECT $1,$2,provider_connection_id,'second-upstream',true FROM deployments WHERE id=$3")
        .bind(deployment).bind(second).bind(d.id).execute(&pool).await.unwrap();
    let unrestricted = s
        .authenticate(&k.personal_key.token)
        .await
        .unwrap()
        .unwrap();
    let second_deployment = s
        .deployments(&unrestricted, &second.to_string())
        .await
        .unwrap()
        .remove(0);
    assert_model_access(
        &s,
        unrestricted,
        &second_deployment,
        &second.to_string(),
        true,
    )
    .await;
    let body = create(&s, &u, k.personal_workspace_id, json!([m])).await;
    let p = principal(&s, &body).await;
    assert_access(&s, p, &d, true).await;
    assert_eq!(s.visible_models(&p).await.unwrap().len(), 1);
    // The cached candidate is real, enabled, and granted, but outside this key's subset.
    assert_model_access(&s, p, &second_deployment, &second.to_string(), false).await;
}

#[sqlx::test]
async fn exactly_two_hundred_distinct_selections_are_accepted_and_listed(pool: PgPool) {
    let (s, k, u, _) = fixture(&pool).await;
    let mut models = Vec::with_capacity(200);
    for _ in 0..200 {
        models.push(model(&pool, k.organization_id, Some(k.personal_workspace_id)).await);
    }
    let body = create(&s, &u, k.personal_workspace_id, json!(models)).await;
    models.sort_unstable();
    assert_eq!(body["model_ids"], json!(models));
    let (status, listed) = call(&s, &u, "GET", &base(k.personal_workspace_id), json!({})).await;
    assert_eq!(status, StatusCode::OK);
    let row = listed["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] == body["id"])
        .unwrap();
    assert_eq!(row["model_ids"], json!(models));
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM key_model_selections WHERE governance_key_id=$1"
        )
        .bind(id(&body))
        .fetch_one(&pool)
        .await
        .unwrap(),
        200
    );
}

#[sqlx::test]
async fn team_and_project_human_and_service_rotation_preserves_restrictions(pool: PgPool) {
    let (s, k, u, _) = fixture(&pool).await;
    let (m, d) = enabled(&pool).await;
    let project = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO workspaces(id,organization_id,name,kind) VALUES($1,$2,'Project','project')",
    )
    .bind(project)
    .bind(k.organization_id)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO workspace_memberships(organization_id,workspace_id,user_id,role) VALUES($1,$2,$3,'owner')").bind(k.organization_id).bind(project).bind(u.user_id).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO workspace_model_grants(organization_id,workspace_id,model_id) VALUES($1,$2,$3)").bind(k.organization_id).bind(project).bind(m).execute(&pool).await.unwrap();
    for ws in [k.team_workspace_id, project] {
        let (status, account) = call(
            &s,
            &u,
            "POST",
            &format!("/api/v1/workspaces/{ws}/service-accounts"),
            json!({"name":"Automation"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        for service in [Value::Null, account["id"].clone()] {
            for selections in [Value::Null, json!([]), json!([m])] {
                let (status, body) = call(&s, &u, "POST", &base(ws), json!({"name":"Shared", "expires_in_days":30,"service_account_id":service,"model_ids":selections})).await;
                assert_eq!(status, StatusCode::OK);
                let old = principal(&s, &body).await;
                let (status, rotated) = call(
                    &s,
                    &u,
                    "POST",
                    &format!("{}/{}/rotate", base(ws), id(&body)),
                    json!({"expires_in_days":30}),
                )
                .await;
                assert_eq!(status, StatusCode::OK);
                assert!(
                    s.authenticate(body["token"].as_str().unwrap())
                        .await
                        .unwrap()
                        .is_none()
                );
                assert_access(&s, old, &d, false).await;
                let ownership: (Uuid, Option<Uuid>, Option<Uuid>) = sqlx::query_as("SELECT governance_key_id,issued_to_user_id,service_account_id FROM api_keys WHERE id=$1").bind(id(&rotated)).fetch_one(&pool).await.unwrap();
                assert_eq!(ownership.0, id(&body));
                assert_eq!(
                    ownership.1,
                    if service.is_null() {
                        Some(u.user_id)
                    } else {
                        None
                    }
                );
                assert_eq!(
                    ownership.2,
                    service.as_str().map(|v| v.parse::<Uuid>().unwrap())
                );
                let listed = call(&s, &u, "GET", &base(ws), json!({})).await.1;
                assert_eq!(
                    listed["data"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .find(|row| row["id"] == rotated["id"])
                        .unwrap()["model_ids"],
                    selections
                );
                assert_access(
                    &s,
                    principal(&s, &rotated).await,
                    &d,
                    selections != json!([]),
                )
                .await;
                // Rotation follows the original header, never creates a fresh permission set.
                assert_eq!(
                    sqlx::query_scalar::<_, i64>(
                        "SELECT count(*) FROM key_model_restrictions WHERE governance_key_id=$1"
                    )
                    .bind(id(&rotated))
                    .fetch_one(&pool)
                    .await
                    .unwrap(),
                    0
                );
            }
        }
    }
}

#[sqlx::test]
async fn restricted_key_lists_preserve_personal_privacy_and_shared_ownership(pool: PgPool) {
    let (s, k, u, mut other) = fixture(&pool).await;
    let (m, _) = enabled(&pool).await;
    add_member(&pool, k.organization_id, k.team_workspace_id, other.user_id).await;
    let private = create(&s, &u, k.personal_workspace_id, json!([m])).await;
    let own = create(&s, &other, k.team_workspace_id, json!([m])).await;
    let another = create(&s, &u, k.team_workspace_id, json!([])).await;
    let (status, account) = call(
        &s,
        &u,
        "POST",
        &format!(
            "/api/v1/workspaces/{}/service-accounts",
            k.team_workspace_id
        ),
        json!({"name":"Service"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, service) = call(&s, &u, "POST", &base(k.team_workspace_id), json!({"name":"Service key","expires_in_days":30,"service_account_id":account["id"],"model_ids":[m]})).await;
    assert_eq!(status, StatusCode::OK);
    let (status, listed) = call(&s, &other, "GET", &base(k.team_workspace_id), json!({})).await;
    assert_eq!(status, StatusCode::OK);
    let rows = listed["data"].as_array().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["id"], own["id"]);
    assert_eq!(rows[0]["model_ids"], json!([m]));
    let (status, listed) = call(&s, &u, "GET", &base(k.team_workspace_id), json!({})).await;
    assert_eq!(status, StatusCode::OK);
    for body in [&own, &another, &service] {
        assert!(
            listed["data"]
                .as_array()
                .unwrap()
                .iter()
                .any(|row| row["id"] == body["id"] && row["model_ids"] == body["model_ids"])
        );
        assert!(!listed.to_string().contains(body["token"].as_str().unwrap()));
    }
    assert!(!listed.to_string().contains("secret_hash"));
    assert!(!listed.to_string().contains(private["id"].as_str().unwrap()));
    // Even a live platform operator/organization admin cannot list another personal scope.
    sqlx::query(
        "UPDATE organization_memberships SET role='admin' WHERE organization_id=$1 AND user_id=$2",
    )
    .bind(k.organization_id)
    .bind(other.user_id)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("UPDATE users SET platform_admin=true WHERE id=$1")
        .bind(other.user_id)
        .execute(&pool)
        .await
        .unwrap();
    other.platform_admin = true;
    assert_eq!(
        call(&s, &other, "GET", &base(k.personal_workspace_id), json!({}))
            .await
            .0,
        StatusCode::FORBIDDEN
    );
}

#[sqlx::test]
async fn creation_waits_for_parent_removal_and_leaves_no_key_or_audit(pool: PgPool) {
    let (s, k, u, _) = fixture(&pool).await;
    let (m, _) = enabled(&pool).await;
    let before = counts(&pool).await;
    let mut tx = pool.begin().await.unwrap();
    resources::catalog_lock(&mut tx, false).await.unwrap();
    sqlx::query("SELECT id FROM organizations WHERE id=$1 FOR NO KEY UPDATE")
        .bind(k.organization_id)
        .execute(&mut *tx)
        .await
        .unwrap();
    for table in [
        "workspace_model_grants",
        "user_model_grants",
        "organization_model_grants",
    ] {
        sqlx::query(&format!(
            "DELETE FROM {table} WHERE organization_id=$1 AND model_id=$2"
        ))
        .bind(k.organization_id)
        .bind(m)
        .execute(&mut *tx)
        .await
        .unwrap();
    }
    let ws = k.personal_workspace_id;
    let task = tokio::spawn(async move {
        call(
            &s,
            &u,
            "POST",
            &base(ws),
            json!({"name":"Racing", "expires_in_days":30,"model_ids":[m]}),
        )
        .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let waiting: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE datname=current_database() AND wait_event_type='Lock' AND query LIKE '%FROM organizations%FOR NO KEY UPDATE%')").fetch_one(&pool).await.unwrap();
            if waiting { break; }
            tokio::task::yield_now().await;
        }
    }).await.unwrap();
    assert!(!task.is_finished());
    tx.commit().await.unwrap();
    let (status, body) = task.await.unwrap();
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(counts(&pool).await, before);
}

#[sqlx::test]
async fn restricted_rotation_does_not_reset_key_budget_holds(pool: PgPool) {
    let (s, k, u, _) = fixture(&pool).await;
    let (m, d) = enabled(&pool).await;
    let body = create(&s, &u, k.personal_workspace_id, json!([m])).await;
    sqlx::query("INSERT INTO deployment_prices(id,deployment_id,input_microusd_per_million,output_microusd_per_million,input_token_limit,output_token_limit) VALUES($1,$2,1000000,1000000,100,50)").bind(Uuid::new_v4()).bind(d.id).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO governance_policies(id,organization_id,scope,workspace_id,api_key_id,monthly_budget_microusd) VALUES($1,$2,'key',$3,$4,110)").bind(Uuid::new_v4()).bind(k.organization_id).bind(k.personal_workspace_id).bind(id(&body)).execute(&pool).await.unwrap();
    let p = principal(&s, &body).await;
    let original = record(p, &d);
    governance::admit_for_deployment(&s, &original, &request(), 30, &d)
        .await
        .unwrap();
    let (status, rotated) = call(
        &s,
        &u,
        "POST",
        &format!("{}/{}/rotate", base(k.personal_workspace_id), id(&body)),
        json!({"expires_in_days":30}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let p = principal(&s, &rotated).await;
    assert_eq!(s.visible_models(&p).await.unwrap().len(), 1);
    let next = record(p, &d);
    assert_eq!(
        governance::admit_for_deployment(&s, &next, &request(), 30, &d).await,
        Err(InferenceError::Busy)
    );
    let held: (i64, i64) =
        sqlx::query_as("SELECT count(*),sum(held_microusd)::bigint FROM governance_reservations")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(held, (1, 110));
    assert_eq!(
        sqlx::query_scalar::<_, Uuid>(
            "SELECT api_key_id FROM governance_reservations WHERE execution_id=$1"
        )
        .bind(original.id)
        .fetch_one(&pool)
        .await
        .unwrap(),
        id(&body)
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM inference_executions WHERE id=$1")
            .bind(next.id)
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
}

#[sqlx::test]
async fn admission_waits_for_parent_revocation_then_denies_without_accounting(pool: PgPool) {
    let (s, k, u, _) = fixture(&pool).await;
    let (m, d) = enabled(&pool).await;
    let body = create(&s, &u, k.personal_workspace_id, json!([m])).await;
    let p = principal(&s, &body).await;
    let mut tx = pool.begin().await.unwrap();
    resources::catalog_lock(&mut tx, false).await.unwrap();
    sqlx::query("SELECT id FROM organizations WHERE id=$1 FOR NO KEY UPDATE")
        .bind(k.organization_id)
        .execute(&mut *tx)
        .await
        .unwrap();
    for table in [
        "workspace_model_grants",
        "user_model_grants",
        "organization_model_grants",
    ] {
        sqlx::query(&format!(
            "DELETE FROM {table} WHERE organization_id=$1 AND model_id=$2"
        ))
        .bind(k.organization_id)
        .bind(m)
        .execute(&mut *tx)
        .await
        .unwrap();
    }
    let rec = record(p, &d);
    let execution = rec.id;
    let task = tokio::spawn(async move {
        governance::admit_for_deployment(&s, &rec, &request(), 30, &d).await
    });
    tokio::time::timeout(std::time::Duration::from_secs(5),async {
        loop {
            let waiting:bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE datname=current_database() AND wait_event_type='Lock' AND query LIKE '%FROM organizations%FOR NO KEY UPDATE%')").fetch_one(&pool).await.unwrap();
            if waiting { break; }
            tokio::task::yield_now().await;
        }
    }).await.unwrap();
    assert!(!task.is_finished());
    tx.commit().await.unwrap();
    assert_eq!(task.await.unwrap(), Err(InferenceError::ModelUnavailable));
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM inference_executions WHERE id=$1")
            .bind(execution)
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM governance_reservations WHERE execution_id=$1"
        )
        .bind(execution)
        .fetch_one(&pool)
        .await
        .unwrap(),
        0
    );
}
