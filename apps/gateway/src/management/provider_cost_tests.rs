//! #48: documented Anthropic-format fixtures, never live provider credentials.
use super::*;

const IMPORTS: &str = "/api/v1/platform/provider-cost-imports";
const MAPPINGS: &str = "/api/v1/platform/provider-cost-mappings";
const RECONCILIATIONS: &str = "/api/v1/platform/provider-reconciliations";

pub(super) fn upload() -> Value {
    json!({
        "account_id":"organization_placeholder", "evidence_reference":"finance:export-1",
        "exported_at":"2025-08-03T00:00:00Z", "supersedes":null,
        "request":{"starting_at":"2025-08-01T00:00:00Z","ending_at":"2025-08-02T00:00:00Z",
                   "bucket_width":"1d","group_by":["workspace_id","description"],"limit":31},
        "pages":[{"data":[{"starting_at":"2025-08-01T00:00:00Z","ending_at":"2025-08-02T00:00:00Z",
          "results":[{"amount":"123.78912","context_window":"0-200k","cost_type":"tokens","currency":"USD",
           "description":"Placeholder model - Input Tokens","inference_geo":"global","model":"claude-placeholder",
           "service_tier":"standard","speed":"standard","token_type":"uncached_input_tokens","workspace_id":"wrkspc_placeholder"}]}],
          "has_more":false,"next_page":null}]
    })
}

pub(super) fn mapping(connection: Uuid, workspace: Option<Uuid>) -> Value {
    json!({"account_id":"organization_placeholder","supersedes":null,"evidence_reference":"finance:binding-1",
      "bindings":[{"provider_workspace_id":"wrkspc_placeholder","connection_id":connection,"deployment_id":null,
      "workspace_id":workspace,"effective_start":"2025-08-01T00:00:00Z","effective_end":"2025-08-02T00:00:00Z",
      "exclusive":workspace.is_some(),"non_omg_traffic":false,"attested":true,"evidence_reference":"finance:attestation-1",
      "model_mappings":[]}]})
}

#[test]
fn provider_cost_parser_exact_signed_half_even_and_residual() {
    use super::super::provider_costs::parser::cents;
    for (input, amount, residual) in [
        ("123.78912", 1237891, "0.2"),
        ("-123.78912", -1237891, "-0.2"),
        ("0.00005", 0, "0.5"),
        ("0.00015", 2, "-0.5"),
        ("-0.00005", 0, "-0.5"),
        ("-0.00015", -2, "0.5"),
        ("1", 10000, "0"),
        ("-1.25", -12500, "0"),
    ] {
        let converted = cents(input).unwrap();
        assert_eq!(converted.0, amount, "{input}");
        assert_eq!(converted.1, residual, "{input}");
    }
    for invalid in [
        "",
        "NaN",
        "1e3",
        "+1",
        " 1",
        "1.",
        ".1",
        "--1",
        "99999999999999999999999999999999999999999999",
    ] {
        assert!(cents(invalid).is_err(), "{invalid}");
    }
}

async fn connection(pool: &PgPool) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO provider_connections(id,name,provider,credential_ref) VALUES($1,'Placeholder','anthropic','env:PLACEHOLDER')")
        .bind(id).execute(pool).await.unwrap();
    id
}

async fn create_import(f: &Fixture, body: Value) -> Value {
    let (status, value) = call(&f.s, &f.admin, "POST", IMPORTS, body).await;
    assert_eq!(status, StatusCode::OK, "{value}");
    value
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn provider_cost_import_exactness_duplicate_supersession_and_concurrent_upload(pool: PgPool) {
    let f = fixture(&pool).await;
    let body = upload();
    let (left, right) = tokio::join!(
        create_import(&f, body.clone()),
        create_import(&f, body.clone())
    );
    assert_eq!(left["id"], right["id"]);
    let first = id(&left);
    let (_, lines) = call(
        &f.s,
        &f.auditor,
        "GET",
        &format!("{IMPORTS}/{first}/lines"),
        Value::Null,
    )
    .await;
    assert_eq!(lines["data"][0]["amount_microusd"], "1237891");
    assert_eq!(lines["data"][0]["original_amount"], "123.78912");
    assert_eq!(lines["data"][0]["conversion_residual_microusd"], "0.2");
    let mut changed = body.clone();
    changed["pages"][0]["data"][0]["results"][0]["amount"] = json!("-2.00015");
    assert_eq!(
        call(&f.s, &f.admin, "POST", IMPORTS, changed.clone())
            .await
            .0,
        StatusCode::CONFLICT
    );
    changed["supersedes"] = json!(first);
    let second = create_import(&f, changed.clone()).await;
    assert_ne!(left["id"], second["id"]);
    assert_eq!(
        create_import(&f, body).await["id"],
        left["id"],
        "an old exact reupload is not a new revision"
    );
    changed["pages"][0]["data"][0]["results"][0]["amount"] = json!("4");
    assert_eq!(
        call(&f.s, &f.admin, "POST", IMPORTS, changed).await.0,
        StatusCode::CONFLICT,
        "cannot branch a revision"
    );
    let supersedes: Uuid =
        sqlx::query_scalar("SELECT supersedes FROM provider_cost_imports WHERE id=$1")
            .bind(id(&second))
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(supersedes, first);
    let mut overlap = upload();
    overlap["request"]["ending_at"] = json!("2025-08-03T00:00:00Z");
    let mut day = overlap["pages"][0]["data"][0].clone();
    day["starting_at"] = json!("2025-08-02T00:00:00Z");
    day["ending_at"] = json!("2025-08-03T00:00:00Z");
    overlap["pages"][0]["data"]
        .as_array_mut()
        .unwrap()
        .push(day);
    assert_eq!(
        call(&f.s, &f.admin, "POST", IMPORTS, overlap).await.0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM provider_cost_imports")
            .fetch_one(&pool)
            .await
            .unwrap(),
        2
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn provider_cost_import_rejects_malformed_incomplete_duplicate_and_oversized_evidence(
    pool: PgPool,
) {
    let f = fixture(&pool).await;
    let mut cases = Vec::new();
    let mut b = upload();
    b["pages"][0]["has_more"] = json!(true);
    cases.push(b);
    let mut b = upload();
    b["pages"][0]["next_page"] = json!("unconsumed");
    cases.push(b);
    let mut b = upload();
    let row = b["pages"][0]["data"][0]["results"][0].clone();
    b["pages"][0]["data"][0]["results"]
        .as_array_mut()
        .unwrap()
        .push(row);
    cases.push(b);
    let mut b = upload();
    let page = b["pages"][0].clone();
    b["pages"].as_array_mut().unwrap().push(page);
    cases.push(b);
    let mut b = upload();
    b["pages"][0]["data"] = json!([]);
    cases.push(b);
    let mut b = upload();
    b["pages"][0]["data"][0]["results"][0]["currency"] = json!("EUR");
    cases.push(b);
    let mut b = upload();
    b["request"]["group_by"] = json!(["description"]);
    cases.push(b);
    let mut b = upload();
    b["exported_at"] = json!("2025-08-01T00:00:00Z");
    cases.push(b);
    let mut b = upload();
    b["evidence_reference"] = json!("x".repeat(257));
    cases.push(b);
    let mut b = upload();
    b["pages"][0]["data"][0]["results"][0]["description"] = json!("x".repeat(4097));
    cases.push(b);
    let mut b = upload();
    let row = b["pages"][0]["data"][0]["results"][0].clone();
    b["pages"][0]["data"][0]["results"] = json!(vec![row; 50_001]);
    cases.push(b);
    for b in cases {
        assert!(
            call(&f.s, &f.admin, "POST", IMPORTS, b)
                .await
                .0
                .is_client_error()
        );
    }
    for raw in [
        upload().to_string().replace(
            "\"amount\":\"123.78912\"",
            "\"amount\":\"1\",\"amount\":\"2\"",
        ),
        format!("{{\"pages\":{} }}", "[".repeat(40) + "0" + &"]".repeat(40)),
        " ".repeat(5 * 1024 * 1024 + 1),
    ] {
        let response = routes()
            .merge(super::super::provider_costs::upload_routes())
            .layer(Extension(f.admin.clone()))
            .with_state(f.s.clone())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(IMPORTS)
                    .header("content-type", "application/json")
                    .body(Body::from(raw))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(response.status().is_client_error());
    }
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM provider_cost_imports")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn provider_cost_mapping_lineage_attestation_and_authority(pool: PgPool) {
    let f = fixture(&pool).await;
    let c = connection(&pool).await;
    for actor in [&f.auditor, &f.owner, &f.member, &f.outsider] {
        for (path, body) in [
            (IMPORTS, upload()),
            (MAPPINGS, mapping(c, Some(f.team))),
            (
                RECONCILIATIONS,
                json!({"import_id":Uuid::new_v4(),"mapping_id":Uuid::new_v4()}),
            ),
        ] {
            assert_eq!(
                call(&f.s, actor, "POST", path, body).await.0,
                StatusCode::FORBIDDEN
            );
        }
    }
    let b = mapping(c, Some(f.team));
    let (status, first) = call(&f.s, &f.admin, "POST", MAPPINGS, b.clone()).await;
    assert_eq!(status, StatusCode::OK, "{first}");
    assert_eq!(
        call(&f.s, &f.admin, "POST", MAPPINGS, b.clone()).await.0,
        StatusCode::CONFLICT
    );
    let mut b2 = b.clone();
    b2["supersedes"] = first["id"].clone();
    assert_eq!(
        call(&f.s, &f.admin, "POST", MAPPINGS, b2).await.0,
        StatusCode::OK
    );
    for (field, value) in [
        ("attested", json!(false)),
        ("non_omg_traffic", json!(true)),
        ("workspace_id", Value::Null),
    ] {
        let mut bad = b.clone();
        bad["account_id"] = json!(format!("bad-{field}"));
        bad["bindings"][0][field] = value;
        assert_eq!(
            call(&f.s, &f.admin, "POST", MAPPINGS, bad).await.0,
            StatusCode::BAD_REQUEST
        );
    }
    for actor in [&f.admin, &f.auditor] {
        assert_eq!(
            call(&f.s, actor, "GET", MAPPINGS, Value::Null).await.0,
            StatusCode::OK
        );
    }
    assert_eq!(
        call(&f.s, &f.owner, "GET", MAPPINGS, Value::Null).await.0,
        StatusCode::FORBIDDEN
    );
    sqlx::query("UPDATE platform_role_grants SET revoked_at=now() WHERE user_id=$1")
        .bind(f.admin.user_id)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        call(&f.s, &f.admin, "POST", IMPORTS, upload()).await.0,
        StatusCode::FORBIDDEN
    );
}

async fn financial_state(pool: &PgPool) -> Value {
    let mut state = serde_json::Map::new();
    for table in [
        "monetary_ledger",
        "governance_reservations",
        "deployment_prices",
        "budget_totals",
        "usage_rollups_hourly",
        "policy_budgets",
    ] {
        let v: Value = sqlx::query_scalar(&format!(
            "SELECT coalesce(jsonb_agg(to_jsonb(t) ORDER BY to_jsonb(t)::text),'[]') FROM {table} t"
        ))
        .fetch_one(pool)
        .await
        .unwrap();
        state.insert(table.into(), v);
    }
    Value::Object(state)
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn provider_cost_reconciliation_aggregate_privacy_shared_unknown_and_estimates_unchanged(
    pool: PgPool,
) {
    let f = fixture(&pool).await;
    let c = connection(&pool).await;
    let m = model(&pool, "placeholder-public-alias").await;
    let d = Uuid::new_v4();
    sqlx::query("INSERT INTO deployments(id,model_id,provider_connection_id,upstream_model) VALUES($1,$2,$3,'claude-placeholder')").bind(d).bind(m).bind(c).execute(&pool).await.unwrap();
    let (_, k) = key(&f, &f.owner, f.personal, Value::Null).await;
    let execution = Uuid::new_v4();
    sqlx::query("INSERT INTO inference_executions(id,workspace_id,api_key_id,deployment_id,public_model,provider,upstream_model,streamed,state,root_request_id,attempt_number,started_at) VALUES($1,$2,$3,$4,'placeholder-public-alias','anthropic','claude-placeholder',false,'indeterminate',$1,1,'2025-08-01T12:00:00Z')")
        .bind(execution).bind(f.personal).bind(id(&k)).bind(d).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO governance_reservations(execution_id,workspace_id,api_key_id,deployment_id,admitted_at,minute_start,month_start,lease_expires_at,state,held_microusd,unbounded_cost) VALUES($1,$2,$3,$4,'2025-08-01T12:00:00Z','2025-08-01T12:00:00Z','2025-08-01T00:00:00Z',now(),'unknown',42,true)")
        .bind(execution).bind(f.personal).bind(id(&k)).bind(d).execute(&pool).await.unwrap();
    let before = financial_state(&pool).await;
    let import = create_import(&f, upload()).await;
    let (status, map) = call(&f.s, &f.admin, "POST", MAPPINGS, mapping(c, None)).await;
    assert_eq!(status, StatusCode::OK, "{map}");
    let (status, rec) = call(
        &f.s,
        &f.admin,
        "POST",
        RECONCILIATIONS,
        json!({"import_id":id(&import),"mapping_id":id(&map)}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{rec}");
    let path = format!("{RECONCILIATIONS}/{}", id(&rec));
    let (_, report) = call(&f.s, &f.auditor, "GET", &path, Value::Null).await;
    let raw = report.to_string();
    assert!(raw.contains("shared_unallocated"), "{raw}");
    assert!(raw.contains("incomplete_estimate"), "{raw}");
    assert!(raw.contains("timing_basis"), "{raw}");
    assert!(!raw.contains(&execution.to_string()));
    assert!(!raw.contains(&f.personal.to_string()));
    assert!(!raw.contains(&id(&k).to_string()));
    assert!(
        report["snapshot"]["rows"]
            .as_array()
            .unwrap()
            .iter()
            .all(|row| row["workspace_estimates"]
                .as_array()
                .is_none_or(Vec::is_empty))
    );
    assert_eq!(
        report["snapshot"]["personal_activity"]["estimates"]["held_floor_microusd"],
        "42"
    );
    assert!(!raw.contains("placeholder-public-alias"));
    assert_eq!(financial_state(&pool).await, before);
    let (_, again) = call(&f.s, &f.auditor, "GET", &path, Value::Null).await;
    assert_eq!(report, again, "snapshot reads never recompute");
    assert_eq!(
        call(&f.s, &f.owner, "GET", &path, Value::Null).await.0,
        StatusCode::FORBIDDEN
    );
    // A historical model is required, not today's deployment alias.
    let mut unknown = upload();
    unknown["account_id"] = json!("unknown-model-account");
    unknown["pages"][0]["data"][0]["results"][0]["model"] = json!("not-observed");
    let unknown_import = create_import(&f, unknown).await;
    let mut unknown_map = mapping(c, Some(f.personal));
    unknown_map["account_id"] = json!("unknown-model-account");
    let (_, unknown_map) = call(&f.s, &f.admin, "POST", MAPPINGS, unknown_map).await;
    let (status, report) = call(
        &f.s,
        &f.admin,
        "POST",
        RECONCILIATIONS,
        json!({"import_id":id(&unknown_import),"mapping_id":id(&unknown_map)}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{report}");
    let (_, report) = call(
        &f.s,
        &f.admin,
        "GET",
        &format!("{RECONCILIATIONS}/{}", id(&report)),
        Value::Null,
    )
    .await;
    assert!(report.to_string().contains("unknown_model"));
    assert!(report.to_string().contains("estimate_only"));
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn provider_cost_pagination_fingerprints_and_incomplete_revision_rollback(pool: PgPool) {
    let f = fixture(&pool).await;
    let mut b = upload();
    b["request"]["ending_at"] = json!("2025-08-03T00:00:00Z");
    b["request"]["limit"] = json!(1);
    let mut next = b["pages"][0].clone();
    next["data"][0]["starting_at"] = json!("2025-08-02T00:00:00Z");
    next["data"][0]["ending_at"] = json!("2025-08-03T00:00:00Z");
    b["pages"][0]["has_more"] = json!(true);
    b["pages"][0]["next_page"] = json!("cursor_placeholder");
    b["pages"].as_array_mut().unwrap().push(next.clone());
    let original = create_import(&f, b.clone()).await;
    b["request"]["limit"] = json!(31);
    let day = next["data"][0].clone();
    b["pages"][0]["data"].as_array_mut().unwrap().push(day);
    b["pages"][0]["has_more"] = json!(false);
    b["pages"][0]["next_page"] = Value::Null;
    b["pages"].as_array_mut().unwrap().pop();
    assert_eq!(
        create_import(&f, b).await["id"],
        original["id"],
        "pagination is provenance, not financial identity"
    );
    let mut tx = crate::db::begin(&pool).await.unwrap();
    sqlx::query("INSERT INTO provider_cost_imports(id,account_id,period_start,period_end,exported_at,actor_user_id,evidence_reference,evidence_json,fingerprint,line_count) VALUES($1,'incomplete','2025-08-01','2025-08-02','2025-08-03',$2,'opaque:rollback','{}',decode(repeat('ff',32),'hex'),1)")
        .bind(Uuid::new_v4()).bind(f.admin.user_id).execute(&mut *tx).await.unwrap();
    assert!(
        tx.commit().await.is_err(),
        "a parent cannot commit without its promised evidence"
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM provider_cost_imports WHERE account_id='incomplete'"
        )
        .fetch_one(&pool)
        .await
        .unwrap(),
        0
    );
    assert!(sqlx::query("INSERT INTO provider_cost_lines(import_id,line_number,source_page,source_pointer,bucket_start,bucket_end,dimensions,original_amount,amount_microusd,conversion_residual_microusd,dimension_key) VALUES($1,3,0,'/late','2025-08-01','2025-08-02','{}','1',10000,'0',repeat('a',64))")
        .bind(id(&original)).execute(&pool).await.is_err(),"cannot append a child to a committed revision");
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn provider_cost_history_append_only_even_for_owner(pool: PgPool) {
    let f = fixture(&pool).await;
    let imported = create_import(&f, upload()).await;
    for statement in [
        "UPDATE provider_cost_imports SET account_id=account_id",
        "DELETE FROM provider_cost_imports",
        "TRUNCATE provider_cost_imports CASCADE",
        "UPDATE provider_cost_lines SET original_amount=original_amount",
        "DELETE FROM provider_cost_lines",
        "TRUNCATE provider_cost_lines",
    ] {
        assert!(
            sqlx::query(statement).execute(&pool).await.is_err(),
            "{statement}"
        );
    }
    assert_eq!(create_import(&f, upload()).await["id"], imported["id"]);
}
