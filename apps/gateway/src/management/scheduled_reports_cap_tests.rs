use super::*;
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn scheduled_reports_scope_caps_concurrent_boundary_resume_and_tombstones(pool: PgPool) {
    let f = fixture(&pool).await;
    let mock = crate::identity::tests::MockProvider::start(None).await;
    let identity = mock.state(pool.clone()).await;
    let cc = Uuid::new_v4();
    sqlx::query("INSERT INTO cost_centers(id,name,code) VALUES($1,'Reports','REPORTS')")
        .bind(cc)
        .execute(&pool)
        .await
        .unwrap();
    for (kind, ws, who, cap) in [
        ("personal", Some(f.personal), &f.owner, 10i64),
        ("workspace", Some(f.team), &f.owner, 50),
        ("installation", None, &f.admin, 100),
    ] {
        let base = ws.map(collection).unwrap_or_else(|| PLATFORM.into());
        // Paused rows consume capacity too; platform cost-center schedules share
        // one bucket with installation schedules, never one bucket per label.
        sqlx::query("INSERT INTO report_schedules(scope_kind,workspace_id,cost_center_id,created_by,cadence,timezone,delivery_time) SELECT CASE WHEN $1='installation' AND g%2=0 THEN 'cost_center' ELSE $1 END,$2,CASE WHEN $1='installation' AND g%2=0 THEN $5 ELSE NULL END,$3,'weekly','UTC','09:00' FROM generate_series(1,$4::bigint) g")
            .bind(kind).bind(ws).bind(who.user_id).bind(cap-1).bind(cc).execute(&pool).await.unwrap();
        let body = input(&[who.user_id]);
        let (a, b) = tokio::join!(
            call(&f.s, who, "POST", &base, body.clone()),
            call(&f.s, who, "POST", &base, body.clone())
        );
        let mut statuses = vec![a.0, b.0];
        statuses.sort();
        assert_eq!(statuses, vec![StatusCode::OK, StatusCode::CONFLICT]);
        let refused = if a.0 == StatusCode::CONFLICT {
            &a.1
        } else {
            &b.1
        };
        assert_eq!(refused["error"]["reason"], "report_schedule_limit_reached");
        let id: Uuid = if a.0 == StatusCode::OK {
            a.1["id"].as_str().unwrap()
        } else {
            b.1["id"].as_str().unwrap()
        }
        .parse()
        .unwrap();
        let path = format!("{base}/{id}");
        let app = routes()
            .layer(Extension(who.clone()))
            .layer(Extension(settings::SignIn(identity.clone())))
            .with_state(f.s.clone());
        let resume = || review::configured_call(&app, "PATCH", &path, json!({"state":"active"}));
        assert_eq!(
            resume().await.0,
            StatusCode::OK,
            "at cap, resume counts itself once"
        );
        assert_eq!(
            call(&f.s, who, "PATCH", &path, json!({"state":"paused"}))
                .await
                .0,
            StatusCode::OK
        );
        let extra:Uuid=sqlx::query_scalar("INSERT INTO report_schedules(scope_kind,workspace_id,created_by,cadence,timezone,delivery_time) VALUES($1,$2,$3,'weekly','UTC','09:00') RETURNING id").bind(kind).bind(ws).bind(who.user_id).fetch_one(&pool).await.unwrap();
        let (status, body) = resume().await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["error"]["reason"], "report_schedule_limit_reached");
        sqlx::query("UPDATE report_schedules SET deleted_at=now() WHERE id=$1")
            .bind(extra)
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(
            resume().await.0,
            StatusCode::OK,
            "tombstones do not consume capacity"
        );
        assert_eq!(
            call(&f.s, who, "DELETE", &path, Value::Null).await.0,
            StatusCode::OK
        );
        assert_eq!(
            call(&f.s, who, "POST", &base, input(&[who.user_id]))
                .await
                .0,
            StatusCode::OK
        );
    }
    assert_eq!(
        call(
            &f.s,
            &f.owner,
            "POST",
            &collection(f.project),
            input(&[f.owner.user_id])
        )
        .await
        .0,
        StatusCode::OK,
        "shared workspace buckets are independent"
    );
}
