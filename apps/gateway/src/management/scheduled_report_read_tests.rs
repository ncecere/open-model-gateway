//! #102C authenticated occurrence reads are independently live-authorized.
use super::*;
use crate::leases::{Lease, Leases};
async fn occurrence(
    f: &Fixture,
    pool: &PgPool,
    owner: &BrowserPrincipal,
    ws: Option<Uuid>,
) -> (Uuid, Uuid, String) {
    occurrence_body(f, pool, owner, ws, None).await
}
pub(super) async fn occurrence_body(
    f: &Fixture,
    pool: &PgPool,
    owner: &BrowserPrincipal,
    ws: Option<Uuid>,
    body: Option<Value>,
) -> (Uuid, Uuid, String) {
    occurrence_body_at(f, pool, owner, ws, body, None).await
}
async fn occurrence_body_at(
    f: &Fixture,
    pool: &PgPool,
    owner: &BrowserPrincipal,
    ws: Option<Uuid>,
    body: Option<Value>,
    now: Option<chrono::DateTime<chrono::Utc>>,
) -> (Uuid, Uuid, String) {
    let kind = if ws == Some(f.personal) {
        "personal"
    } else if ws.is_some() {
        "workspace"
    } else {
        "installation"
    };
    let schedule: Uuid = sqlx::query_scalar("INSERT INTO report_schedules(scope_kind,workspace_id,created_by,cadence,timezone,delivery_time,state) VALUES($1,$2,$3,'weekly','UTC','00:01','active') RETURNING id").bind(kind).bind(ws).bind(owner.user_id).fetch_one(pool).await.unwrap();
    sqlx::query("INSERT INTO report_schedule_recipients(schedule_id,slot,user_id) VALUES($1,1,$2)")
        .bind(schedule)
        .bind(owner.user_id)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("WITH fixture_now AS MATERIALIZED (SELECT coalesce($2::timestamptz,clock_timestamp()) AS t) UPDATE report_schedules SET next_period_end=date_trunc('week',t,'UTC')-interval '7 days',next_run_at=t-interval '1 day' FROM fixture_now WHERE id=$1").bind(schedule).bind(now).execute(pool).await.unwrap();
    let leases = Leases::new();
    leases.renew_once(pool).await;
    let fence = leases.held(Lease::ReportScheduler).unwrap();
    let c = match now {
        Some(now) => crate::scheduled_reports::claim_at(&f.s, &fence, schedule, now).await,
        None => crate::scheduled_reports::claim(&f.s, &fence, schedule).await,
    }
    .unwrap()
    .unwrap();
    let body = match body {
        Some(mut body) => {
            let observed: chrono::DateTime<chrono::Utc> =
                sqlx::query_scalar("SELECT clock_timestamp()")
                    .fetch_one(pool)
                    .await
                    .unwrap();
            body["observed_at"] = json!(observed);
            body
        }
        None => crate::scheduled_reports::snapshot(&f.s, &c).await.unwrap(),
    };
    assert!(
        crate::scheduled_reports::complete(&f.s, &fence, &c, &body)
            .await
            .unwrap()
    );
    sqlx::query("SELECT omg_lease_release('report_scheduler',$1,$2)")
        .bind(fence.holder)
        .bind(fence.epoch)
        .execute(pool)
        .await
        .unwrap();
    let run: Uuid = sqlx::query_scalar("SELECT id FROM report_runs WHERE schedule_id=$1")
        .bind(schedule)
        .fetch_one(pool)
        .await
        .unwrap();
    let base = ws.map_or_else(
        || "/api/v1/platform".into(),
        |ws| format!("/api/v1/workspaces/{ws}"),
    );
    (
        schedule,
        run,
        format!("{base}/report-schedules/{schedule}/runs/{run}"),
    )
}
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn scheduled_reports_read_authority_personal_privacy_auditor_and_live_revocation(
    pool: PgPool,
) {
    let f = fixture(&pool).await;
    let (_, _, platform) = occurrence(&f, &pool, &f.admin, None).await;
    let response = routes()
        .layer(Extension(ManagementPrincipal::Browser(f.admin.clone())))
        .layer(Extension(f.admin.clone()))
        .with_state(f.s.clone())
        .oneshot(
            Request::builder()
                .uri(&platform)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["cache-control"], "no-store");
    assert_eq!(response.headers()["x-content-type-options"], "nosniff");
    let (_, _, team) = occurrence(&f, &pool, &f.owner, Some(f.team)).await;
    let (_, _, personal) = occurrence(&f, &pool, &f.owner, Some(f.personal)).await;
    for who in [&f.admin, &f.auditor] {
        assert_eq!(
            call(&f.s, who, "GET", &platform, Value::Null).await.0,
            StatusCode::OK
        );
    }
    for who in [&f.owner, &f.member, &f.outsider] {
        assert_eq!(
            call(&f.s, who, "GET", &platform, Value::Null).await.0,
            StatusCode::FORBIDDEN
        );
    }
    assert_eq!(
        call(&f.s, &f.owner, "GET", &team, Value::Null).await.0,
        StatusCode::OK
    );
    for who in [&f.admin, &f.auditor, &f.member, &f.outsider] {
        assert_eq!(
            call(&f.s, who, "GET", &team, Value::Null).await.0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            call(&f.s, who, "GET", &personal, Value::Null).await.0,
            StatusCode::NOT_FOUND
        );
    }
    assert_eq!(
        call(&f.s, &f.owner, "GET", &personal, Value::Null).await.0,
        StatusCode::OK
    );
    let unknown = team.rsplit_once('/').unwrap().0.to_owned() + "/" + &Uuid::new_v4().to_string();
    assert_eq!(
        call(&f.s, &f.owner, "GET", &unknown, Value::Null).await.0,
        StatusCode::NOT_FOUND
    );
    sqlx::query("UPDATE workspace_membership_grants SET revoked_at=clock_timestamp() WHERE workspace_id=$1 AND user_id=$2")
        .bind(f.team)
        .bind(f.owner.user_id)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        call(&f.s, &f.owner, "GET", &team, Value::Null).await.0,
        StatusCode::FORBIDDEN
    );
    sqlx::query("UPDATE platform_role_grants SET revoked_at=clock_timestamp() WHERE user_id=$1")
        .bind(f.auditor.user_id)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        call(&f.s, &f.auditor, "GET", &platform, Value::Null)
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    for route in [
        "/api/v1/platform/report-schedules/{id}/runs/{run}",
        "/api/v1/workspaces/{ws}/report-schedules/{id}/runs/{run}",
    ] {
        assert!(matches!(
            crate::route_policy::token_policy("GET", route).unwrap(),
            crate::route_policy::TokenPolicy::Denied
        ));
    }
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn scheduled_reports_read_uses_one_clock_at_expiry(pool: PgPool) {
    let mut f = fixture(&pool).await;
    let past = sqlx::query_scalar(
        "SELECT date_trunc('week',clock_timestamp(),'UTC') - interval '7 days 12 hours'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    let (schedule, _, detail) =
        occurrence_body_at(&f, &pool, &f.admin, None, None, Some(past)).await;
    // Shadow only this test database's unqualified clock reads. The first two
    // reads precede expiry; subsequent reads cross it, without sleeps or races.
    // Session settings work even in the handler's read-only transaction.
    sqlx::raw_sql(
        "CREATE SCHEMA test_clock;
         CREATE FUNCTION test_clock.clock_timestamp() RETURNS timestamptz
         LANGUAGE plpgsql VOLATILE AS $$
         DECLARE n int; expiry timestamptz; mode text;
         BEGIN
           n := coalesce(nullif(current_setting('test_clock.reads',true),''),'0')::int + 1;
           PERFORM set_config('test_clock.reads',n::text,false);
           mode := current_setting('test_clock.mode',true);
           SELECT min(expires_at) INTO STRICT expiry FROM public.report_runs;
           IF mode = 'boundary' THEN RETURN expiry; END IF;
           IF mode = 'expired' OR (mode = 'moving' AND n > 2) THEN
             RETURN expiry + interval '1 second';
           END IF;
           RETURN expiry - interval '1 second';
         END $$;",
    )
    .execute(&pool)
    .await
    .unwrap();
    let clock_pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect_with(
            (*pool.connect_options())
                .clone()
                .options([("search_path", "test_clock,public,pg_catalog")]),
        )
        .await
        .unwrap();
    f.s.pool = clock_pool.clone();
    for (mode, expired) in [
        ("moving", false),
        ("live", false),
        ("boundary", true),
        ("expired", true),
    ] {
        sqlx::query("SELECT set_config('test_clock.reads','0',false), set_config('test_clock.mode',$1,false)")
            .bind(mode).execute(&clock_pool).await.unwrap();
        let (status, body) = call(&f.s, &f.admin, "GET", &detail, Value::Null).await;
        assert_eq!(status, StatusCode::OK, "run detail status");
        assert!(
            body["expired"].as_bool() == Some(expired),
            "run expiry classification"
        );
        assert!(
            body["snapshot"].is_null() == expired,
            "run snapshot expiry consistency"
        );
        let reads: String = sqlx::query_scalar("SELECT current_setting('test_clock.reads')")
            .fetch_one(&clock_pool)
            .await
            .unwrap();
        assert!(reads == "1", "run detail must evaluate the clock once");
    }
    let list = detail.rsplit_once('/').unwrap().0;
    for (mode, expired) in [("live", false), ("boundary", true), ("expired", true)] {
        sqlx::query("SELECT set_config('test_clock.reads','0',false), set_config('test_clock.mode',$1,false)")
            .bind(mode).execute(&clock_pool).await.unwrap();
        let (status, body) = call(&f.s, &f.admin, "GET", list, Value::Null).await;
        assert_eq!(status, StatusCode::OK, "run list status");
        assert!(
            body["data"].as_array().is_some_and(|rows| rows.len() == 1),
            "run list count"
        );
        assert!(
            body["data"]
                .as_array()
                .unwrap()
                .iter()
                .all(|row| row["expired"].as_bool() == Some(expired)),
            "run list expiry classification"
        );
        let reads: String = sqlx::query_scalar("SELECT current_setting('test_clock.reads')")
            .fetch_one(&clock_pool)
            .await
            .unwrap();
        assert!(reads == "1", "run list must evaluate the clock once");
    }
    // Generate a second legitimate occurrence so per-row clock reads cannot
    // accidentally satisfy the list's one-read-per-statement assertion.
    let leases = Leases::new();
    leases.renew_once(&pool).await;
    let fence = leases.held(Lease::ReportScheduler).unwrap();
    let now = past + chrono::Duration::days(7);
    let claim =
        crate::scheduled_reports::claim_at(&Store::new(pool.clone()), &fence, schedule, now)
            .await
            .unwrap()
            .unwrap();
    let body = crate::scheduled_reports::snapshot(&Store::new(pool.clone()), &claim)
        .await
        .unwrap();
    assert!(
        crate::scheduled_reports::complete(&Store::new(pool.clone()), &fence, &claim, &body)
            .await
            .unwrap(),
        "second run completion"
    );
    sqlx::query("SELECT omg_lease_release('report_scheduler',$1,$2)")
        .bind(fence.holder)
        .bind(fence.epoch)
        .execute(&pool)
        .await
        .unwrap();
    {
        let (mode, expired) = ("live", false);
        sqlx::query("SELECT set_config('test_clock.reads','0',false), set_config('test_clock.mode',$1,false)")
            .bind(mode).execute(&clock_pool).await.unwrap();
        let (status, body) = call(&f.s, &f.admin, "GET", list, Value::Null).await;
        assert_eq!(status, StatusCode::OK, "run list status");
        assert!(
            body["data"].as_array().is_some_and(|rows| rows.len() == 2),
            "run list count"
        );
        assert!(
            body["data"]
                .as_array()
                .unwrap()
                .iter()
                .all(|row| row["expired"].as_bool() == Some(expired)),
            "run list expiry classification"
        );
        let reads: String = sqlx::query_scalar("SELECT current_setting('test_clock.reads')")
            .fetch_one(&clock_pool)
            .await
            .unwrap();
        assert!(reads == "1", "run list must evaluate the clock once");
    }
    // Exercise the running HTTP gateway too, including live authorization refusal.
    let app = routes()
        .layer(Extension(ManagementPrincipal::Browser(f.admin.clone())))
        .layer(Extension(f.admin.clone()))
        .with_state(f.s.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await });
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let response = client
        .get(format!("http://{address}{detail}"))
        .send()
        .await
        .unwrap_or_else(|_| panic!("run HTTP transport"));
    assert_eq!(response.status().as_u16(), 200, "run HTTP status");
    let body: Value = response
        .json()
        .await
        .unwrap_or_else(|_| panic!("run HTTP JSON"));
    assert!(
        body["expired"] == false && !body["snapshot"].is_null(),
        "run HTTP live snapshot"
    );
    sqlx::query("UPDATE platform_role_grants SET revoked_at=clock_timestamp() WHERE user_id=$1")
        .bind(f.admin.user_id)
        .execute(&pool)
        .await
        .unwrap();
    let response = client
        .get(format!("http://{address}{detail}"))
        .send()
        .await
        .unwrap_or_else(|_| panic!("revoked run HTTP transport"));
    assert_eq!(response.status().as_u16(), 403, "revoked run HTTP status");
    server.abort();
    clock_pool.close().await;
}
