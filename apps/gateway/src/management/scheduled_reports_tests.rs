//! #102 slice A: real PostgreSQL and served-router authorization, no SMTP worker.
use super::*;
use crate::route_policy::{self, TokenPolicy};
#[path = "scheduled_reports_cap_tests.rs"]
mod caps;
#[path = "scheduled_reports_review_tests.rs"]
mod review;
const PLATFORM: &str = "/api/v1/platform/report-schedules";
fn input(recipients: &[Uuid]) -> Value {
    json!({"cadence":"weekly","timezone":"UTC","delivery_time":"09:00","state":"paused","recipient_user_ids":recipients})
}
fn collection(ws: Uuid) -> String {
    format!("/api/v1/workspaces/{ws}/report-schedules")
}
async fn create(f: &Fixture, u: &BrowserPrincipal, path: &str, recipients: &[Uuid]) -> Uuid {
    let (status, value) = call(&f.s, u, "POST", path, input(recipients)).await;
    assert_eq!(status, StatusCode::OK, "{value}");
    value["id"].as_str().unwrap().parse().unwrap()
}
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn scheduled_reports_crud_authority_and_personal_existence_hiding(pool: PgPool) {
    let f = fixture(&pool).await;
    let ws_admin = user(&pool, "schedule-admin", "user").await;
    member(&pool, f.team, ws_admin.user_id, "admin").await;
    let platform_id = create(
        &f,
        &f.admin,
        PLATFORM,
        &[f.admin.user_id, f.auditor.user_id],
    )
    .await;
    let team = collection(f.team);
    let team_id = create(&f, &f.owner, &team, &[f.owner.user_id, ws_admin.user_id]).await;
    let personal = collection(f.personal);
    let personal_id = create(&f, &f.owner, &personal, &[f.owner.user_id]).await;
    for (base, id, allowed, readers, denied) in [
        (
            PLATFORM.to_owned(),
            platform_id,
            vec![&f.admin],
            vec![&f.auditor],
            vec![&f.owner, &f.member, &f.outsider, &ws_admin],
        ),
        (
            team.clone(),
            team_id,
            vec![&f.owner, &ws_admin],
            vec![],
            vec![&f.admin, &f.auditor, &f.member, &f.outsider],
        ),
        (
            personal.clone(),
            personal_id,
            vec![&f.owner],
            vec![],
            vec![&f.admin, &f.auditor, &f.member, &f.outsider, &ws_admin],
        ),
    ] {
        let detail = format!("{base}/{id}");
        for who in allowed.iter().chain(readers.iter()) {
            for path in [&base, &detail] {
                assert_eq!(
                    call(&f.s, who, "GET", path, Value::Null).await.0,
                    StatusCode::OK
                );
            }
        }
        for who in allowed {
            assert_eq!(
                call(
                    &f.s,
                    who,
                    "PATCH",
                    &detail,
                    json!({"cadence":"monthly","state":"paused"})
                )
                .await
                .0,
                StatusCode::OK
            );
        }
        for who in readers {
            for (method, path) in [("POST", &base), ("PATCH", &detail), ("DELETE", &detail)] {
                assert_eq!(
                    call(&f.s, who, method, path, input(&[who.user_id])).await.0,
                    StatusCode::FORBIDDEN
                );
            }
        }
        for who in denied {
            for (method, path) in [
                ("GET", &base),
                ("POST", &base),
                ("GET", &detail),
                ("PATCH", &detail),
                ("DELETE", &detail),
            ] {
                let result = call(&f.s, who, method, path, input(&[who.user_id])).await;
                if base == personal {
                    let unknown =
                        path.replace(&f.personal.to_string(), &Uuid::new_v4().to_string());
                    assert_eq!(
                        result,
                        call(&f.s, who, method, &unknown, input(&[who.user_id])).await,
                        "foreign personal = unknown: {method}"
                    );
                    assert_eq!(result.0, StatusCode::NOT_FOUND);
                } else {
                    assert_eq!(
                        result.0,
                        StatusCode::FORBIDDEN,
                        "{method} {path}: {}",
                        result.1
                    );
                }
            }
        }
    }
    // Scope is immutable, IDs never select a different workspace or authority domain.
    for (base, id, who) in [
        (&team, personal_id, &f.owner),
        (&personal, team_id, &f.owner),
        (&PLATFORM.to_owned(), team_id, &f.admin),
    ] {
        for method in ["GET", "PATCH", "DELETE"] {
            assert_eq!(
                call(
                    &f.s,
                    who,
                    method,
                    &format!("{base}/{id}"),
                    json!({"state":"paused"})
                )
                .await
                .0,
                StatusCode::NOT_FOUND
            );
        }
    }
    for ws in [f.team, f.project, f.personal] {
        let base = collection(ws);
        let id = create(&f, &f.owner, &base, &[f.owner.user_id]).await;
        let detail = format!("{base}/{id}");
        assert_eq!(
            call(&f.s, &f.owner, "DELETE", &detail, Value::Null).await.0,
            StatusCode::OK
        );
        assert_eq!(
            call(&f.s, &f.owner, "GET", &detail, Value::Null).await.0,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            call(&f.s, &f.owner, "DELETE", &detail, Value::Null).await.0,
            StatusCode::NOT_FOUND
        );
    }
}
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn scheduled_reports_recipient_validation_live_revocation_and_audit(pool: PgPool) {
    let f = fixture(&pool).await;
    let base = collection(f.team);
    let id = create(&f, &f.owner, &base, &[f.owner.user_id]).await;
    let detail = format!("{base}/{id}");
    let revoked = user(&pool, "report-revoked", "user").await;
    member(&pool, f.team, revoked.user_id, "admin").await;
    sqlx::query("UPDATE workspace_membership_grants SET revoked_at=now() WHERE user_id=$1")
        .bind(revoked.user_id)
        .execute(&pool)
        .await
        .unwrap();
    let suspended = user(&pool, "report-suspended", "admin").await;
    member(&pool, f.team, suspended.user_id, "admin").await;
    sqlx::query("UPDATE users SET disabled_at=now() WHERE id=$1")
        .bind(suspended.user_id)
        .execute(&pool)
        .await
        .unwrap();
    let unentitled = user(&pool, "report-unentitled", "user").await;
    member(&pool, f.team, unentitled.user_id, "admin").await;
    sqlx::query("UPDATE platform_role_grants SET revoked_at=now() WHERE user_id=$1")
        .bind(unentitled.user_id)
        .execute(&pool)
        .await
        .unwrap();
    for recipients in [
        vec![],
        vec![f.member.user_id],
        vec![f.admin.user_id],
        vec![f.auditor.user_id],
        vec![f.outsider.user_id],
        vec![revoked.user_id],
        vec![suspended.user_id],
        vec![unentitled.user_id],
        vec![Uuid::new_v4()],
        vec![f.owner.user_id; 51],
        vec![f.owner.user_id; 2],
    ] {
        assert_eq!(
            call(&f.s, &f.owner, "POST", &base, input(&recipients))
                .await
                .0,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            call(
                &f.s,
                &f.owner,
                "PATCH",
                &detail,
                json!({"recipient_user_ids":recipients})
            )
            .await
            .0,
            StatusCode::BAD_REQUEST
        );
    }
    for recipient in [
        f.owner.user_id,
        f.member.user_id,
        revoked.user_id,
        suspended.user_id,
        unentitled.user_id,
    ] {
        assert_eq!(
            call(&f.s, &f.admin, "POST", PLATFORM, input(&[recipient]))
                .await
                .0,
            StatusCode::BAD_REQUEST
        );
    }
    assert_eq!(
        call(
            &f.s,
            &f.owner,
            "POST",
            &collection(f.personal),
            input(&[f.admin.user_id])
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    for mutation in [
        json!({"recipient_user_ids":["outside@example.test"]}),
        json!({"notify_emails":["outside@example.test"]}),
        json!({"scope_kind":"installation"}),
        json!({"workspace_id":f.project}),
        json!({"cadence":"daily"}),
        json!({"timezone":"not/a-zone"}),
        json!({"delivery_time":"24:00"}),
        json!({"state":"deleted"}),
        json!({"state":null}),
        json!({}),
    ] {
        assert!(
            call(&f.s, &f.owner, "PATCH", &detail, mutation)
                .await
                .0
                .is_client_error()
        );
    }
    assert_eq!(
        call(
            &f.s,
            &f.owner,
            "PATCH",
            &detail,
            json!({"cadence":"monthly"})
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        call(&f.s, &f.owner, "DELETE", &detail, Value::Null).await.0,
        StatusCode::OK
    );
    let rows: Vec<(String, Value)> = sqlx::query_as(
        "SELECT action,metadata FROM audit_events WHERE resource_id=$1 ORDER BY created_at,id",
    )
    .bind(id)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        rows.iter().map(|r| r.0.as_str()).collect::<Vec<_>>(),
        [
            "report_schedule.created",
            "report_schedule.updated",
            "report_schedule.deleted"
        ]
    );
    for (_, metadata) in rows {
        assert_eq!(metadata.as_object().unwrap().len(), 2);
        assert_eq!(metadata["kind"], "workspace");
        assert_eq!(metadata["count"], 1);
    }
    // Direct-handler recheck, not trust in cached BrowserPrincipal flags.
    sqlx::query("UPDATE workspace_membership_grants SET revoked_at=now() WHERE user_id=$1 AND workspace_id=$2").bind(f.owner.user_id).bind(f.team).execute(&pool).await.unwrap();
    assert_eq!(
        call(&f.s, &f.owner, "POST", &base, input(&[f.owner.user_id]))
            .await
            .0,
        StatusCode::FORBIDDEN
    );
}
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn scheduled_reports_historical_cost_center_scope_and_recipient_cap(pool: PgPool) {
    let f = fixture(&pool).await;
    let cc = Uuid::new_v4();
    sqlx::query("INSERT INTO cost_centers(id,name,code,archived_at) VALUES($1,'Historical','HISTORY',now())").bind(cc).execute(&pool).await.unwrap();
    for (base, who, recipient, kind) in [
        (
            PLATFORM.to_owned(),
            &f.admin,
            f.auditor.user_id,
            "cost_center",
        ),
        (collection(f.team), &f.owner, f.owner.user_id, "workspace"),
    ] {
        let mut body = input(&[recipient]);
        body["cost_center_id"] = json!(cc);
        let (status, v) = call(&f.s, who, "POST", &base, body).await;
        assert_eq!(status, StatusCode::OK, "{v}");
        assert_eq!(v["scope_kind"], kind);
        assert_eq!(v["cost_center_id"], cc.to_string());
        let next = v["next_run_at"].as_str().expect("calendar initialized");
        assert!(next.parse::<chrono::DateTime<chrono::Utc>>().is_ok());
    }
    let mut body = input(&[f.owner.user_id]);
    body["cost_center_id"] = json!(cc);
    assert_eq!(
        call(&f.s, &f.owner, "POST", &collection(f.personal), body)
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    let mut active = input(&[f.admin.user_id]);
    active["state"] = json!("active");
    assert_eq!(
        call(&f.s, &f.admin, "POST", PLATFORM, active).await.0,
        StatusCode::CONFLICT,
        "active configuration needs the startup-validated operator origin, not a request destination"
    );
    let mut recipients = vec![f.admin.user_id];
    for n in 0..49 {
        recipients.push(
            user(&pool, &format!("report-cap-{n}"), "auditor")
                .await
                .user_id,
        );
    }
    let id = create(&f, &f.admin, PLATFORM, &recipients).await;
    recipients.push(f.auditor.user_id);
    assert_eq!(
        call(
            &f.s,
            &f.admin,
            "PATCH",
            &format!("{PLATFORM}/{id}"),
            json!({"recipient_user_ids":recipients})
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
}
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn scheduled_reports_every_route_real_sessions_csrf_origin_and_admin_token_refusal(
    pool: PgPool,
) {
    use sha2::{Digest, Sha256};
    let f = fixture(&pool).await;
    let mock = crate::identity::tests::MockProvider::start(None).await;
    let identity = mock.state(pool.clone()).await;
    let app = super::super::router(identity).with_state(f.s.clone());
    let cookie = "c".repeat(64);
    let csrf = "d".repeat(64);
    sqlx::query("INSERT INTO browser_sessions(token_hash,user_id,csrf_hash,expires_at,verified_email) VALUES($1,$2,$3,now()+interval '1 hour',$4)").bind(Sha256::digest(cookie.as_bytes()).as_slice()).bind(f.admin.user_id).bind(Sha256::digest(csrf.as_bytes()).as_slice()).bind(&f.admin.email).execute(&pool).await.unwrap();
    let token = {
        let mut tx = crate::db::begin(&pool).await.unwrap();
        locks::exclusive(&mut tx, [locks::Scope::User(f.admin.user_id)])
            .await
            .unwrap();
        let t = crate::admin_tokens::create_admin_token(
            &mut tx,
            f.admin.user_id,
            "reports",
            &crate::admin_tokens::SCOPES
                .iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>(),
            30,
            None,
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
        t.token
    };
    member(&pool, f.team, f.admin.user_id, "admin").await;
    for base in [PLATFORM.to_owned(), collection(f.team)] {
        let id = create(&f, &f.admin, &base, &[f.admin.user_id]).await;
        for (method, path, template) in [
            ("GET", base.clone(), base.clone()),
            ("POST", base.clone(), base.clone()),
            ("GET", format!("{base}/{id}"), format!("{base}/{{id}}")),
            ("PATCH", format!("{base}/{id}"), format!("{base}/{{id}}")),
            ("DELETE", format!("{base}/{id}"), format!("{base}/{{id}}")),
        ] {
            let template = template.replace(&f.team.to_string(), "{ws}");
            assert_eq!(
                route_policy::token_policy(method, &template),
                Some(TokenPolicy::Denied)
            );
            for (bearer, browser, valid_csrf, origin, expected) in [
                (true, false, false, "", 403),
                (false, false, false, "", 401),
                (
                    false,
                    true,
                    false,
                    "http://127.0.0.1:3000",
                    if method == "GET" { 200 } else { 403 },
                ),
                (
                    false,
                    true,
                    true,
                    "https://evil.test",
                    if method == "GET" { 200 } else { 403 },
                ),
            ] {
                let mut req = Request::builder()
                    .method(method)
                    .uri(&path)
                    .header("content-type", "application/json")
                    .header("origin", origin);
                if bearer {
                    req = req.header("authorization", format!("Bearer {token}"));
                }
                if browser {
                    req = req.header("cookie", format!("omg_session={cookie}"));
                }
                if valid_csrf {
                    req = req.header("x-csrf-token", &csrf);
                }
                let response = app
                    .clone()
                    .oneshot(
                        req.body(Body::from(input(&[f.admin.user_id]).to_string()))
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                assert_eq!(response.status().as_u16(), expected, "{method} {path}");
            }
        }
        let mut body = input(&[f.admin.user_id]);
        body["state"] = json!("active");
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(&base)
                    .header("cookie", format!("omg_session={cookie}"))
                    .header("origin", "http://127.0.0.1:3000")
                    .header("x-csrf-token", &csrf)
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::OK,
            "validated operator public URL allows active inert configuration"
        );
        let created: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 1_000_000).await.unwrap())
                .unwrap();
        let id = created["id"].as_str().unwrap();
        for (method, body) in [
            ("PATCH", json!({"state":"paused"})),
            ("PATCH", json!({"state":"active"})),
            ("DELETE", Value::Null),
        ] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method(method)
                        .uri(format!("{base}/{id}"))
                        .header("cookie", format!("omg_session={cookie}"))
                        .header("origin", "http://127.0.0.1:3000")
                        .header("x-csrf-token", &csrf)
                        .header("content-type", "application/json")
                        .body(Body::from(body.to_string()))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
        }
        let actions: Vec<String> = sqlx::query_scalar(
            "SELECT action FROM audit_events WHERE resource_id=$1 ORDER BY created_at,id",
        )
        .bind(id.parse::<Uuid>().unwrap())
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            actions,
            [
                "report_schedule.created",
                "report_schedule.paused",
                "report_schedule.resumed",
                "report_schedule.deleted"
            ]
        );
    }
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM alert_deliveries")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0,
        "schedule management never manufactures alert deliveries"
    );
}
