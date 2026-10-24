//! Standard-review regressions for #102 slice A (real PostgreSQL).
use super::*;

pub(super) async fn configured_call(
    app: &Router,
    method: &str,
    path: &str,
    body: Value,
) -> (StatusCode, Value) {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let body = to_bytes(response.into_body(), 1_000_000).await.unwrap();
    (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn scheduled_reports_review_pause_survives_recipient_authority_loss(pool: PgPool) {
    let f = fixture(&pool).await;
    let mock = crate::identity::tests::MockProvider::start(None).await;
    let identity = mock.state(pool.clone()).await;
    let app = routes()
        .layer(Extension(f.owner.clone()))
        .layer(Extension(settings::SignIn(identity)))
        .with_state(f.s.clone());
    let base = collection(f.team);
    for loss in ["revoked", "suspended", "demoted"] {
        let recipient = user(&pool, &format!("pause-{loss}"), "user").await;
        member(&pool, f.team, recipient.user_id, "admin").await;
        let id = create(&f, &f.owner, &base, &[recipient.user_id]).await;
        // Fixture starts active, without needing generation or SMTP.
        sqlx::query("UPDATE report_schedules SET state='active' WHERE id=$1")
            .bind(id)
            .execute(&pool)
            .await
            .unwrap();
        let statement = match loss {
            "revoked" => {
                "UPDATE workspace_membership_grants SET revoked_at=clock_timestamp() WHERE user_id=$1"
            }
            "suspended" => "UPDATE users SET disabled_at=clock_timestamp() WHERE id=$1",
            "demoted" => "UPDATE workspace_membership_grants SET role='member' WHERE user_id=$1",
            _ => unreachable!(),
        };
        sqlx::query(statement)
            .bind(recipient.user_id)
            .execute(&pool)
            .await
            .unwrap();
        let path = format!("{base}/{id}");
        let (status, paused) =
            configured_call(&app, "PATCH", &path, json!({"state":"paused"})).await;
        assert_eq!(status, StatusCode::OK, "{loss}: {paused}");
        assert_eq!(paused["state"], "paused");
        assert_eq!(paused["recipient_user_ids"], json!([recipient.user_id]));
        let (status, body) = configured_call(&app, "PATCH", &path, json!({"state":"active"})).await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "resume must reauthorize {loss}: {body}"
        );
        assert_eq!(
            configured_call(
                &app,
                "PATCH",
                &path,
                json!({"recipient_user_ids":[recipient.user_id]})
            )
            .await
            .0,
            StatusCode::BAD_REQUEST,
            "explicitly setting an ineligible recipient is still refused while paused"
        );
        let actions: Vec<String> = sqlx::query_scalar(
            "SELECT action FROM audit_events WHERE resource_id=$1 ORDER BY created_at,id",
        )
        .bind(id)
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            actions,
            ["report_schedule.created", "report_schedule.paused"],
            "one transactional pause audit, no refused resume mutation"
        );
        assert_eq!(
            call(&f.s, &f.owner, "GET", &path, Value::Null).await.1["state"],
            "paused"
        );
    }
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn scheduled_reports_review_revision_tracks_configuration_not_noops(pool: PgPool) {
    let f = fixture(&pool).await;
    let recipient = user(&pool, "revision-recipient", "user").await;
    member(&pool, f.team, recipient.user_id, "admin").await;
    let base = collection(f.team);
    let id = create(&f, &f.owner, &base, &[f.owner.user_id]).await;
    let path = format!("{base}/{id}");
    for (body, revision) in [
        (json!({"state":"paused"}), "1"),
        (json!({"cadence":"weekly"}), "1"),
        (
            json!({"recipient_user_ids":[f.owner.user_id,recipient.user_id]}),
            "2",
        ),
        (
            json!({"recipient_user_ids":[f.owner.user_id,recipient.user_id]}),
            "2",
        ),
        (
            json!({"recipient_user_ids":[recipient.user_id,f.owner.user_id]}),
            "2",
        ),
        (json!({"cadence":"monthly"}), "3"),
        (
            json!({"timezone":"America/New_York","delivery_time":"10:30"}),
            "4",
        ),
        (json!({"recipient_user_ids":[recipient.user_id]}), "5"),
    ] {
        let (status, value) = call(&f.s, &f.owner, "PATCH", &path, body).await;
        assert_eq!(status, StatusCode::OK, "{value}");
        assert_eq!(value["configuration_revision"], revision);
    }
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn scheduled_reports_review_pagination_order_bounds_and_scope_safety(pool: PgPool) {
    let f = fixture(&pool).await;
    let foreign_personal =
        workspace(&pool, "Foreign private", "personal", f.outsider.user_id).await;
    let scopes = [
        (PLATFORM.to_owned(), None, "installation", &f.admin),
        (collection(f.team), Some(f.team), "workspace", &f.owner),
        (
            collection(f.project),
            Some(f.project),
            "workspace",
            &f.owner,
        ),
        (
            collection(f.personal),
            Some(f.personal),
            "personal",
            &f.owner,
        ),
        (
            collection(foreign_personal),
            Some(foreign_personal),
            "personal",
            &f.outsider,
        ),
    ];
    let mut expected = Vec::new();
    for (scope, (base, ws, kind, who)) in scopes.iter().enumerate() {
        let mut ids = Vec::new();
        for n in 0..7 {
            // Interleave all scopes; reverse creation time so ID ordering is
            // distinguishable from the previous created_at index ordering.
            let id = Uuid::from_u128(100 + (n * 10 + scope) as u128);
            sqlx::query("INSERT INTO report_schedules(id,scope_kind,workspace_id,created_by,cadence,timezone,delivery_time,created_at) VALUES($1,$2,$3,$4,'weekly','UTC','09:00',clock_timestamp()-make_interval(days=>$5))")
                .bind(id).bind(kind).bind(ws).bind(who.user_id).bind(n as i32).execute(&pool).await.unwrap();
            sqlx::query(
                "INSERT INTO report_schedule_recipients(schedule_id,slot,user_id) VALUES($1,1,$2)",
            )
            .bind(id)
            .bind(who.user_id)
            .execute(&pool)
            .await
            .unwrap();
            if n == 2 {
                assert_eq!(
                    call(&f.s, who, "DELETE", &format!("{base}/{id}"), Value::Null)
                        .await
                        .0,
                    StatusCode::OK
                );
            } else {
                ids.push(id);
            }
        }
        expected.push(ids);
    }
    for (scope, (base, _, _, who)) in scopes.iter().enumerate() {
        let mut after = None;
        let mut seen = Vec::new();
        for page in 0..3 {
            let path = after.map_or_else(
                || format!("{base}?limit=2"),
                |id| format!("{base}?limit=2&after={id}"),
            );
            let (status, value) = call(&f.s, who, "GET", &path, Value::Null).await;
            assert_eq!(status, StatusCode::OK, "{value}");
            let ids = page_ids(&value);
            assert_eq!(ids, expected[scope][page * 2..page * 2 + 2]);
            assert_eq!(value["has_more"], page < 2);
            if page < 2 {
                assert_eq!(value["next_cursor"], ids.last().unwrap().to_string());
            } else {
                assert!(
                    value["next_cursor"].is_null(),
                    "terminal page has no continuation"
                );
            }
            after = value["next_cursor"].as_str().map(str::to_owned);
            seen.extend(ids);
        }
        assert_eq!(
            seen, expected[scope],
            "no duplicates, omissions or deleted rows"
        );
        assert_eq!(
            seen.iter().collect::<std::collections::HashSet<_>>().len(),
            seen.len()
        );
        for (query, len, more) in [
            ("", 6, false),
            ("?limit=1", 1, true),
            ("?limit=100", 6, false),
        ] {
            let (status, page) =
                call(&f.s, who, "GET", &format!("{base}{query}"), Value::Null).await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(page["data"].as_array().unwrap().len(), len);
            assert_eq!(page["has_more"], more);
        }
        for query in [
            "limit=0",
            "limit=-1",
            "limit=101",
            "limit=garbage",
            "limit=1.5",
            "limit=9223372036854775808",
            "limit=",
            "after=not-a-uuid",
            "after=",
            "after=00000000-0000-0000-0000-00000000000z",
        ] {
            assert!(
                call(&f.s, who, "GET", &format!("{base}?{query}"), Value::Null)
                    .await
                    .0
                    .is_client_error(),
                "{query}"
            );
        }
        let (_, empty) = call(
            &f.s,
            who,
            "GET",
            &format!("{base}?after=ffffffff-ffff-ffff-ffff-ffffffffffff"),
            Value::Null,
        )
        .await;
        assert_eq!(empty["data"], json!([]));
        assert_eq!(empty["has_more"], false);
        assert!(empty["next_cursor"].is_null());
        // Any scope's continuation remains just a position. It never alters
        // the receiving scope's SQL visibility predicate, even across personal.
        for source in &expected {
            let cursor = source[1];
            let (_, page) = call(
                &f.s,
                who,
                "GET",
                &format!("{base}?after={cursor}"),
                Value::Null,
            )
            .await;
            assert_eq!(
                page_ids(&page),
                expected[scope]
                    .iter()
                    .copied()
                    .filter(|id| *id > cursor)
                    .collect::<Vec<_>>()
            );
        }
    }
    let cursor = expected[0][1];
    for who in [&f.admin, &f.auditor, &f.owner] {
        assert_eq!(
            call(
                &f.s,
                who,
                "GET",
                &format!("{}?after={cursor}", collection(foreign_personal)),
                Value::Null
            )
            .await
            .0,
            StatusCode::NOT_FOUND
        );
    }
}
fn page_ids(value: &Value) -> Vec<Uuid> {
    value["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["id"].as_str().unwrap().parse().unwrap())
        .collect()
}
