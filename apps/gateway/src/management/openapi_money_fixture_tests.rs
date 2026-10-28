//! Slice 4: executed, populated, raw-wire money contracts over a running gateway.
use super::*;
use openapi::responses::{Captured, PositiveFixtures, capture_http};
use sha2::{Digest, Sha256};

struct Server(tokio::task::JoinHandle<()>);
impl Drop for Server {
    fn drop(&mut self) {
        self.0.abort();
    }
}
struct Browser {
    token: String,
    csrf: String,
}
async fn browser(pool: &PgPool, user: &BrowserPrincipal) -> Browser {
    let token = crate::entropy::hex_token().unwrap();
    let csrf = crate::entropy::hex_token().unwrap();
    sqlx::query("INSERT INTO browser_sessions(token_hash,user_id,csrf_hash,expires_at,verified_email,last_seen_at) VALUES($1,$2,$3,now()+interval '1 hour',$4,now())")
        .bind(Sha256::digest(token.as_bytes()).as_slice()).bind(user.user_id)
        .bind(Sha256::digest(csrf.as_bytes()).as_slice()).bind(&user.email).execute(pool).await.unwrap();
    Browser { token, csrf }
}
async fn send(
    client: &reqwest::Client,
    base: &str,
    actor: Option<&Browser>,
    method: &str,
    path: &str,
    input: Option<Value>,
) -> Captured {
    let mut r = client
        .request(method.parse().unwrap(), format!("{base}{path}"))
        .header("origin", "http://127.0.0.1:3000");
    if let Some(b) = actor {
        r = r
            .header("cookie", format!("omg_session={}", b.token))
            .header("x-csrf-token", &b.csrf);
    }
    if let Some(input) = input {
        r = r.json(&input);
    }
    capture_http(method, path, r.send().await.unwrap()).await
}
fn body(r: &Captured) -> Value {
    assert_eq!(
        r.status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&r.bytes)
    );
    serde_json::from_slice(&r.bytes).unwrap()
}

async fn observe(
    seen: &mut PositiveFixtures,
    client: &reqwest::Client,
    base: &str,
    actor: &Browser,
    method: &str,
    path: &str,
    input: Option<Value>,
) -> Value {
    let r = send(client, base, Some(actor), method, path, input).await;
    let value = body(&r);
    seen.observe(&r);
    value
}
async fn seed_attempt(
    pool: &PgPool,
    ws: Uuid,
    key: Uuid,
    deployment: Uuid,
    price: Uuid,
    settled: bool,
    days: i32,
) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO inference_executions(id,workspace_id,api_key_id,deployment_id,public_model,provider,streamed,state,root_request_id,attempt_number,started_at,input_tokens,output_tokens,upstream_model,cost_center_id,cost_center_name,cost_center_code) SELECT $1,$2,$3,$4,'money-contract','anthropic',false,'succeeded',$1,1,clock_timestamp()-make_interval(days=>$5),CASE WHEN $6 THEN 3 END,CASE WHEN $6 THEN 2 END,'claude-placeholder',w.cost_center_id,c.name,c.code FROM workspaces w LEFT JOIN cost_centers c ON c.id=w.cost_center_id WHERE w.id=$2")
        .bind(id).bind(ws).bind(key).bind(deployment).bind(days).bind(settled).execute(pool).await.unwrap();
    sqlx::query("INSERT INTO governance_reservations(execution_id,workspace_id,api_key_id,deployment_id,price_id,admitted_at,minute_start,month_start,lease_expires_at,state,held_microusd,actual_microusd,unbounded_cost,input_tokens,output_tokens) SELECT id,workspace_id,api_key_id,deployment_id,$2,started_at,date_trunc('minute',started_at,'UTC'),date_trunc('month',started_at,'UTC'),started_at,CASE WHEN $3 THEN 'settled' ELSE 'unknown' END,10,CASE WHEN $3 THEN CASE WHEN $4=1 THEN 9007199254740993 ELSE 5 END END,false,CASE WHEN $3 THEN 3 END,CASE WHEN $3 THEN 2 END FROM inference_executions WHERE id=$1")
        .bind(id).bind(price).bind(settled).bind(days).execute(pool).await.unwrap();
    sqlx::query("INSERT INTO monetary_ledger(id,execution_id,admitted_at,kind,amount_microusd,created_at) SELECT $1,execution_id,admitted_at,CASE WHEN $3 THEN 'settlement' ELSE 'unknown' END,actual_microusd,admitted_at FROM governance_reservations WHERE execution_id=$2")
        .bind(Uuid::new_v4()).bind(id).bind(settled).execute(pool).await.unwrap();
    id
}
async fn generate_run(f: &Fixture, schedule: Uuid) -> Uuid {
    use crate::leases::{Lease, Leases};
    sqlx::query("UPDATE report_schedules SET state='active' WHERE id=$1")
        .bind(schedule)
        .execute(&f.s.pool)
        .await
        .unwrap();
    sqlx::query("UPDATE report_schedules SET next_period_end=date_trunc('week',clock_timestamp(),'UTC')-interval '7 days',next_run_at=clock_timestamp()-interval '1 day' WHERE id=$1")
        .bind(schedule).execute(&f.s.pool).await.unwrap();
    let leases = Leases::new();
    leases.renew_once(&f.s.pool).await;
    let fence = leases.held(Lease::ReportScheduler).unwrap();
    let claim = crate::scheduled_reports::claim(&f.s, &fence, schedule)
        .await
        .unwrap()
        .unwrap();
    let snapshot = crate::scheduled_reports::snapshot(&f.s, &claim)
        .await
        .unwrap();
    assert!(
        snapshot["totals"]["attempts"]
            .as_str()
            .unwrap()
            .parse::<u64>()
            .unwrap()
            > 0
    );
    assert!(
        crate::scheduled_reports::complete(&f.s, &fence, &claim, &snapshot)
            .await
            .unwrap()
    );
    sqlx::query("SELECT omg_lease_release('report_scheduler',$1,$2)")
        .bind(fence.holder)
        .bind(fence.epoch)
        .execute(&f.s.pool)
        .await
        .unwrap();
    sqlx::query_scalar("SELECT id FROM report_runs WHERE schedule_id=$1")
        .bind(schedule)
        .fetch_one(&f.s.pool)
        .await
        .unwrap()
}
fn represented(r: &Captured, seen: &mut PositiveFixtures) -> Value {
    let v = body(r);
    seen.observe(r);
    v
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn management_openapi_slice4_money_fixtures(pool: PgPool) {
    let f = fixture(&pool).await;
    let admin = browser(&pool, &f.admin).await;
    let auditor = browser(&pool, &f.auditor).await;
    let owner = browser(&pool, &f.owner).await;
    let member_browser = browser(&pool, &f.member).await;
    // Reconciliation requires both live platform-write and workspace membership.
    member(&pool, f.team, f.admin.user_id, "admin").await;
    let model_id = model(&pool, "money-contract").await;
    for ws in [f.team, f.personal] {
        direct(&pool, ws, model_id).await;
    }
    let connection = Uuid::new_v4();
    let deployment = Uuid::new_v4();
    let price = Uuid::new_v4();
    sqlx::query("INSERT INTO provider_connections(id,name,provider,credential_ref,enabled) VALUES($1,'Money contract','anthropic','env:UNUSED',true)")
        .bind(connection).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO deployments(id,model_id,provider_connection_id,upstream_model,enabled) VALUES($1,$2,$3,'claude-placeholder',true)")
        .bind(deployment).bind(model_id).bind(connection).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO deployment_prices(id,deployment_id,input_microusd_per_million,output_microusd_per_million,input_token_limit,output_token_limit,pricing_version) VALUES($1,$2,1000000,1000000,1000,100,1)")
        .bind(price).bind(deployment).execute(&pool).await.unwrap();
    let (_, team_key) = key(&f, &f.owner, f.team, Value::Null).await;
    let (_, private_key) = key(&f, &f.owner, f.personal, Value::Null).await;
    let (_, member_key) = key(&f, &f.member, f.team, Value::Null).await;

    let mock = crate::identity::tests::MockProvider::start(None).await;
    let identity = mock.state(pool.clone()).await;
    let engine = crate::inference::Engine::new(
        std::sync::Arc::new(f.s.clone()),
        crate::providers::ProviderRegistry::default(),
        crate::inference::EngineLimits::default(),
    )
    .unwrap();
    let app = crate::http::router_with_identity(f.s.clone(), None, engine, identity);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let _server = Server(tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap()
    }));
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let mut seen = PositiveFixtures::default();
    let centers = "/api/v1/platform/cost-centers";
    let cc = id(&observe(
        &mut seen,
        &client,
        &base,
        &admin,
        "POST",
        centers,
        Some(json!({"name":"Admission snapshot","code":"MONEY"})),
    )
    .await);
    sqlx::query("UPDATE workspaces SET cost_center_id=$1 WHERE id=$2")
        .bind(cc)
        .bind(f.team)
        .execute(&pool)
        .await
        .unwrap();
    seed_attempt(&pool, f.team, id(&team_key), deployment, price, true, 1).await;
    let unresolved = seed_attempt(&pool, f.team, id(&team_key), deployment, price, false, 3).await;
    let private = seed_attempt(
        &pool,
        f.personal,
        id(&private_key),
        deployment,
        price,
        false,
        3,
    )
    .await;
    sqlx::query("UPDATE governance_reservations SET held_microusd=NULL,unbounded_cost=true WHERE execution_id=$1").bind(private).execute(&pool).await.unwrap();
    let own = seed_attempt(&pool, f.team, id(&member_key), deployment, price, true, 2).await;
    // Always populate the latest completed weekly period, irrespective of
    // which weekday this test runs on. Settled rows do not grow the hold queue.
    seed_attempt(&pool, f.team, id(&team_key), deployment, price, true, 7).await;
    seed_attempt(
        &pool,
        f.personal,
        id(&private_key),
        deployment,
        price,
        true,
        7,
    )
    .await;
    for ws in [f.team, f.personal] {
        sqlx::query("INSERT INTO storage_usage_hours(workspace_id,purpose,hour_start,byte_seconds,file_count) VALUES($1,'user_file',date_trunc('hour',clock_timestamp())-interval '1 day',3600000,1)")
            .bind(ws).execute(&pool).await.unwrap();
    }
    for p in [centers.to_owned(), format!("{centers}/{cc}")] {
        let v = observe(&mut seen, &client, &base, &auditor, "GET", &p, None).await;
        if p == centers {
            assert!(!v["data"].as_array().unwrap().is_empty());
        }
    }
    observe(
        &mut seen,
        &client,
        &base,
        &admin,
        "PATCH",
        &format!("{centers}/{cc}"),
        Some(json!({"name":"Renamed label","code":"RENAMED"})),
    )
    .await;
    let now = chrono::Utc::now().date_naive();
    let start = (now - chrono::TimeDelta::days(7)).to_string();
    let end = (now + chrono::TimeDelta::days(1)).to_string();
    let range = format!("start_date={start}&end_date={end}");
    let wsbase = format!("/api/v1/workspaces/{}", f.team);
    for (actor, scope) in [
        (&auditor, "/api/v1/platform".to_owned()),
        (&owner, wsbase.clone()),
    ] {
        for suffix in [
            "cost-report",
            "usage/overview",
            "usage/explore",
            "usage/storage",
        ] {
            let extra = if suffix == "usage/explore" {
                "&metric=spend&group_by=key"
            } else {
                ""
            };
            let v = observe(
                &mut seen,
                &client,
                &base,
                actor,
                "GET",
                &format!("{scope}/{suffix}?{range}{extra}"),
                None,
            )
            .await;
            if suffix == "cost-report" {
                assert!(
                    v["totals"]["attempts"]
                        .as_str()
                        .unwrap()
                        .parse::<u64>()
                        .unwrap()
                        > 0
                );
            }
            if suffix == "usage/explore" {
                assert!(!v["rows"].as_array().unwrap().is_empty());
                if scope.contains("/platform") {
                    assert!(
                        v["rows"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .any(|r| r["group"]["id"].is_null()
                                && r["group"]["name"] == "Personal workspace keys")
                    );
                    assert!(!v.to_string().contains(private_key["id"].as_str().unwrap()));
                }
            }
            if suffix == "usage/overview" && scope.contains("/platform") {
                assert!(
                    v["top"]["keys"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|r| r["id"].is_null() && r["name"] == "Personal workspace keys")
                );
                assert!(
                    !v["top"]["keys"]
                        .to_string()
                        .contains(private_key["id"].as_str().unwrap())
                );
            }
            if suffix == "usage/storage" {
                assert_ne!(v["total"]["byte_seconds"], "0");
            }
        }
    }
    observe(
        &mut seen,
        &client,
        &base,
        &auditor,
        "GET",
        "/api/v1/platform/overview",
        None,
    )
    .await;
    for suffix in [
        "cost-summary".to_owned(),
        "usage".to_owned(),
        format!("costs?{range}"),
    ] {
        let v = observe(
            &mut seen,
            &client,
            &base,
            &owner,
            "GET",
            &format!("{wsbase}/{suffix}"),
            None,
        )
        .await;
        if suffix.starts_with("costs?") {
            let rows = v["data"].as_array().unwrap();
            assert!(!rows.is_empty());
            assert!(
                rows.iter()
                    .all(|r| r["cost_center_name"] == "Admission snapshot"
                        && r["cost_center_code"] == "MONEY")
            );
            assert!(rows.iter().any(|r| r["cost_microusd"].is_null()));
        }
    }
    // Member own activity is filtered before grouping and pagination.
    let v = observe(
        &mut seen,
        &client,
        &base,
        &member_browser,
        "GET",
        &format!("{wsbase}/costs?{range}"),
        None,
    )
    .await;
    assert_eq!(v["data"].as_array().unwrap().len(), 1);
    assert_eq!(v["data"][0]["id"], json!(own));
    for actor in [&admin, &auditor] {
        let v = observe(
            &mut seen,
            &client,
            &base,
            actor,
            "GET",
            "/api/v1/platform/holds",
            None,
        )
        .await;
        assert_eq!(v["personal"]["count"], "1");
        assert!(v["personal"]["known_floor_microusd"].is_null());
        assert_eq!(v["personal"]["unbounded_count"], "1");
        for secret in [private, f.personal, id(&private_key)] {
            assert!(!v.to_string().contains(&secret.to_string()));
        }
        assert_eq!(v["data"][0]["can_reconcile"], actor.token == admin.token);
    }
    let v = observe(
        &mut seen,
        &client,
        &base,
        &owner,
        "GET",
        &format!("{wsbase}/holds"),
        None,
    )
    .await;
    assert_eq!(v["data"][0]["execution_id"], json!(unresolved));
    assert!(v["personal"].is_null());
    let v = observe(
        &mut seen,
        &client,
        &base,
        &owner,
        "GET",
        &format!("/api/v1/workspaces/{}/holds", f.personal),
        None,
    )
    .await;
    assert_eq!(v["data"][0]["execution_id"], json!(private));
    assert_eq!(v["data"][0]["workspace"]["kind"], "personal");
    assert!(v["personal"].is_null());
    let r = send(
        &client,
        &base,
        Some(&owner),
        "GET",
        &format!("{wsbase}/usage-export?{range}"),
        None,
    )
    .await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(
        std::str::from_utf8(&r.bytes)
            .unwrap()
            .contains("Admission snapshot")
    );
    seen.observe(&r);
    observe(&mut seen,&client,&base,&admin,"POST",&format!("{wsbase}/costs/{unresolved}/reconcile"),Some(json!({"input_tokens":"3","output_tokens":"2","billing_usage":null,"evidence":"contract:receipt"}))).await;
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT actual_microusd FROM governance_reservations WHERE execution_id=$1"
        )
        .bind(unresolved)
        .fetch_one(&pool)
        .await
        .unwrap(),
        5
    );

    for (actor, scope, recipient) in [
        (&admin, "/api/v1/platform".to_owned(), f.admin.user_id),
        (&owner, wsbase.clone(), f.owner.user_id),
    ] {
        let schedules = format!("{scope}/report-schedules");
        let schedule=id(&observe(&mut seen,&client,&base,actor,"POST",&schedules,Some(json!({"cadence":"weekly","timezone":"UTC","delivery_time":"00:01","recipient_user_ids":[recipient],"cost_center_id":if scope.contains("/platform") {None} else {Some(cc)}}))).await);
        let path = format!("{schedules}/{schedule}");
        observe(
            &mut seen,
            &client,
            &base,
            actor,
            "PATCH",
            &path,
            Some(json!({"delivery_time":"00:02"})),
        )
        .await;
        observe(&mut seen, &client, &base, actor, "GET", &path, None).await;
        let v = observe(&mut seen, &client, &base, actor, "GET", &schedules, None).await;
        assert!(!v["data"].as_array().unwrap().is_empty());
        let run = generate_run(&f, schedule).await;
        let runpath = format!("{path}/runs/{run}");
        let v = observe(
            &mut seen,
            &client,
            &base,
            actor,
            "GET",
            &format!("{path}/runs"),
            None,
        )
        .await;
        assert!(!v["data"].as_array().unwrap().is_empty());
        let v = observe(&mut seen, &client, &base, actor, "GET", &runpath, None).await;
        assert_eq!(v["expired"], false);
        assert_eq!(v["snapshot"]["held_is_floor"], true);
        if scope.contains("/platform") {
            assert!(!v["snapshot"].to_string().contains(&f.personal.to_string()));
            assert!(
                v["snapshot"]["breakdowns"]["workspaces"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|r| r["id"].is_null() && r["name"] == "Personal (aggregate)")
            );
        }
        let run_schema = json!({"$ref":"#/components/schemas/MoneyReportRun"});
        let mut wrong = v.clone();
        wrong["snapshot"] = Value::Null;
        assert!(
            openapi::responses::validate_value(openapi::responses::artifact(), &run_schema, &wrong)
                .is_err()
        );
        let mut wrong = v.clone();
        wrong["expired"] = json!(true);
        assert!(
            openapi::responses::validate_value(openapi::responses::artifact(), &run_schema, &wrong)
                .is_err()
        );
        let r = send(
            &client,
            &base,
            Some(actor),
            "GET",
            &format!("{runpath}/csv"),
            None,
        )
        .await;
        assert_eq!(r.status, StatusCode::OK);
        assert!(std::str::from_utf8(&r.bytes).unwrap().lines().count() > 1);
        seen.observe(&r);
        // Test-only time travel in this fresh fixture: production keeps the
        // occurrence append-only. Restore the trigger before any HTTP read.
        let mut tx = pool.begin().await.unwrap();
        sqlx::query("ALTER TABLE report_runs DISABLE TRIGGER report_runs_immutable")
            .execute(&mut *tx)
            .await
            .unwrap();
        sqlx::query(
            "UPDATE report_runs SET expires_at=clock_timestamp()-interval '1 second' WHERE id=$1",
        )
        .bind(run)
        .execute(&mut *tx)
        .await
        .unwrap();
        sqlx::query("ALTER TABLE report_runs ENABLE TRIGGER report_runs_immutable")
            .execute(&mut *tx)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        let v = observe(&mut seen, &client, &base, actor, "GET", &runpath, None).await;
        assert_eq!(v["expired"], true);
        assert!(v["snapshot"].is_null());
        let r = send(
            &client,
            &base,
            Some(actor),
            "GET",
            &format!("{runpath}/csv"),
            None,
        )
        .await;
        assert_eq!(r.status, StatusCode::GONE);
        observe(&mut seen, &client, &base, actor, "DELETE", &path, None).await;
        assert_eq!(
            send(&client, &base, Some(actor), "GET", &runpath, None)
                .await
                .status,
            StatusCode::NOT_FOUND
        );
    }
    // Provider upload uses actual bytes, preserving duplicate-key evidence.
    let imports = "/api/v1/platform/provider-cost-imports";
    let mappings = "/api/v1/platform/provider-cost-mappings";
    let reconciliations = "/api/v1/platform/provider-reconciliations";
    seed_attempt(
        &pool,
        f.personal,
        id(&private_key),
        deployment,
        price,
        false,
        1,
    )
    .await;
    let mut upload = provider_cost_tests::upload();
    let from = format!("{}T00:00:00Z", now - chrono::TimeDelta::days(1));
    let until = format!("{now}T00:00:00Z");
    upload["request"]["starting_at"] = json!(from);
    upload["request"]["ending_at"] = json!(until);
    upload["exported_at"] = json!(until);
    upload["pages"][0]["data"][0]["starting_at"] = json!(from);
    upload["pages"][0]["data"][0]["ending_at"] = json!(until);
    let mut credit = upload["pages"][0]["data"][0]["results"][0].clone();
    credit["amount"] = json!("-2.00015");
    credit["cost_type"] = json!("credits");
    upload["pages"][0]["data"][0]["results"]
        .as_array_mut()
        .unwrap()
        .push(credit);
    let r = capture_http(
        "POST",
        imports,
        client
            .post(format!("{base}{imports}"))
            .header("origin", "http://127.0.0.1:3000")
            .header("cookie", format!("omg_session={}", admin.token))
            .header("x-csrf-token", &admin.csrf)
            .header("content-type", "application/json")
            .body(serde_json::to_vec(&upload).unwrap())
            .send()
            .await
            .unwrap(),
    )
    .await;
    let import = id(&represented(&r, &mut seen));
    let mapping = id(&observe(
        &mut seen,
        &client,
        &base,
        &admin,
        "POST",
        mappings,
        Some({
            let mut mapping = provider_cost_tests::mapping(connection, Some(f.team));
            mapping["bindings"][0]["effective_start"] = json!(from);
            mapping["bindings"][0]["effective_end"] = json!(until);
            mapping
        }),
    )
    .await);
    let recon = id(&observe(
        &mut seen,
        &client,
        &base,
        &admin,
        "POST",
        reconciliations,
        Some(json!({"import_id":import,"mapping_id":mapping})),
    )
    .await);
    for path in [
        imports.to_owned(),
        format!("{imports}/{import}"),
        format!("{imports}/{import}/lines"),
        mappings.to_owned(),
        format!("{mappings}/{mapping}"),
        reconciliations.to_owned(),
        format!("{reconciliations}/{recon}"),
    ] {
        let v = observe(&mut seen, &client, &base, &auditor, "GET", &path, None).await;
        if let Some(data) = v.get("data") {
            assert!(!data.as_array().unwrap().is_empty());
        }
        if path.ends_with(&recon.to_string()) {
            assert!(v["snapshot"]["estimates"]["holds_upper_bound_microusd"].is_null());
            assert!(!v["snapshot"]["rows"].as_array().unwrap().is_empty());
            assert!(v["snapshot"]["rows"].as_array().unwrap().iter().any(|r| {
                r["imported_microusd"]
                    .as_str()
                    .is_some_and(|s| s.starts_with('-'))
            }));
            assert_eq!(
                v["snapshot"]["personal_activity"]["estimates"]["attempts"],
                "1"
            );
            for secret in [f.personal, id(&private_key), private] {
                assert!(!v["snapshot"].to_string().contains(&secret.to_string()));
            }
        }
    }
    let duplicate = serde_json::to_string(&upload).unwrap().replace(
        "\"next_page\":null",
        "\"next_page\":null,\"next_page\":null",
    );
    let r = capture_http(
        "POST",
        imports,
        client
            .post(format!("{base}{imports}"))
            .header("origin", "http://127.0.0.1:3000")
            .header("cookie", format!("omg_session={}", admin.token))
            .header("x-csrf-token", &admin.csrf)
            .header("content-type", "application/json")
            .body(duplicate)
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    observe(
        &mut seen,
        &client,
        &base,
        &admin,
        "DELETE",
        &format!("{centers}/{cc}"),
        None,
    )
    .await;
    // Real refusals: unauthenticated, inference credential, foreign personal and Auditor writes.
    assert_eq!(
        send(&client, &base, None, "GET", centers, None)
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );
    let r = capture_http(
        "GET",
        centers,
        client
            .get(format!("{base}{centers}"))
            .bearer_auth(team_key["token"].as_str().unwrap())
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    for actor in [&admin, &auditor] {
        assert_eq!(
            send(
                &client,
                &base,
                Some(actor),
                "GET",
                &format!("/api/v1/workspaces/{}/costs?{range}", f.personal),
                None
            )
            .await
            .status,
            StatusCode::NOT_FOUND
        );
    }
    assert_eq!(
        send(
            &client,
            &base,
            Some(&auditor),
            "POST",
            centers,
            Some(json!({"name":"Denied","code":"DENIED"}))
        )
        .await
        .status,
        StatusCode::FORBIDDEN
    );
    // Real extractor refusal controls follow successful controls for each DTO.
    let schedule_body = json!({"cadence":"weekly","timezone":"UTC","delivery_time":"00:01","recipient_user_ids":[f.admin.user_id],"unexpected_configuration":true});
    for (path, input, status) in [
        (
            centers.to_owned(),
            json!({"name":"Nope","code":"NOPE","unexpected_configuration":true}),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            "/api/v1/platform/report-schedules".to_owned(),
            schedule_body,
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            mappings.to_owned(),
            json!({"account_id":"account","evidence_reference":"receipt","bindings":[{"unexpected_configuration":true}]}),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            reconciliations.to_owned(),
            json!({"import_id":import,"mapping_id":mapping,"unexpected_configuration":true}),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            format!("{wsbase}/costs/{unresolved}/reconcile"),
            json!({"input_tokens":"3","output_tokens":"2","evidence":"receipt"}),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            format!("{wsbase}/costs/{unresolved}/reconcile"),
            json!({"input_tokens":"3","output_tokens":"2","billing_usage":{"unexpected_configuration":true},"evidence":"receipt"}),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
    ] {
        assert_eq!(
            send(&client, &base, Some(&admin), "POST", &path, Some(input))
                .await
                .status,
            status
        );
    }
    let r = capture_http(
        "POST",
        centers,
        client
            .post(format!("{base}{centers}"))
            .header("origin", "http://127.0.0.1:3000")
            .header("cookie", format!("omg_session={}", admin.token))
            .json(&json!({"name":"No CSRF","code":"NO-CSRF"}))
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    seen.finish(openapi::responses::SLICE4_AREAS);
}
