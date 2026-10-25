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
    sqlx::query("UPDATE report_schedules SET next_period_end=date_trunc('week',clock_timestamp(),'UTC')-interval '7 days',next_run_at=clock_timestamp()-interval '1 day' WHERE id=$1").bind(schedule).execute(pool).await.unwrap();
    let leases = Leases::new();
    leases.renew_once(pool).await;
    let fence = leases.held(Lease::ReportScheduler).unwrap();
    let c = crate::scheduled_reports::claim(&f.s, &fence, schedule)
        .await
        .unwrap()
        .unwrap();
    let body = match body {
        Some(body) => body,
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
