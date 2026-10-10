//! Scale plan P1: reports, usage, logs, `/me` and `/v1/models` read from
//! lock-free `REPEATABLE READ READ ONLY` snapshots (`crate::reporting`). They
//! neither wait for nor block the catalog/installation locks, keep their
//! authorization live per request, and may use an optional reporting pool
//! with a primary fallback.
use super::*;
use std::time::Duration;

/// Seeded activity: an enabled deployment, a key of `owner` and `member` in
/// the team and one of `owner` in the personal workspace, one execution each.
struct Activity {
    team_key: String,
    member_token: String,
}
async fn activity(f: &Fixture, pool: &PgPool) -> Activity {
    let m = model(pool, "snapshot-model").await;
    for ws in [f.team, f.personal] {
        direct(pool, ws, m).await;
    }
    let provider = Uuid::new_v4();
    let deployment = Uuid::new_v4();
    sqlx::query("INSERT INTO provider_connections(id,name,provider,credential_ref,enabled) VALUES($1,'Mock','openai','env:TEST',true)").bind(provider).execute(pool).await.unwrap();
    sqlx::query("INSERT INTO deployments(id,model_id,provider_connection_id,upstream_model,enabled) VALUES($1,$2,$3,'Mock',true)").bind(deployment).bind(m).bind(provider).execute(pool).await.unwrap();
    let mut tokens = Vec::new();
    for (actor, ws) in [
        (&f.owner, f.team),
        (&f.member, f.team),
        (&f.owner, f.personal),
    ] {
        let (status, k) = key(f, actor, ws, Value::Null).await;
        assert_eq!(status, StatusCode::OK, "{k}");
        let execution = Uuid::new_v4();
        sqlx::query("INSERT INTO inference_executions(id,workspace_id,api_key_id,deployment_id,public_model,provider,streamed,state,root_request_id,input_tokens,output_tokens) VALUES($1,$2,$3,$4,'snapshot-model','openai',false,'succeeded',$1,5,1)").bind(execution).bind(ws).bind(id(&k)).bind(deployment).execute(pool).await.unwrap();
        tokens.push(k["token"].as_str().unwrap().to_owned());
    }
    Activity {
        team_key: tokens[0].clone(),
        member_token: tokens[1].clone(),
    }
}
fn range() -> String {
    let end = chrono::Utc::now().date_naive() + chrono::TimeDelta::days(1);
    format!(
        "start_date={}&end_date={end}",
        end - chrono::TimeDelta::days(7)
    )
}
/// Every lock-free read endpoint as (who, path).
fn reads(f: &Fixture) -> Vec<(&BrowserPrincipal, String)> {
    let (team, personal, r) = (f.team, f.personal, range());
    let mut out = vec![
        (&f.auditor, format!("/api/v1/platform/cost-report?{r}")),
        (&f.auditor, format!("/api/v1/platform/usage/overview?{r}")),
        (
            &f.auditor,
            format!("/api/v1/platform/usage/explore?{r}&metric=spend&group_by=workspace"),
        ),
        (&f.auditor, format!("/api/v1/platform/logs/requests?{r}")),
        (&f.auditor, format!("/api/v1/platform/logs/generations?{r}")),
        (&f.auditor, format!("/api/v1/platform/logs/sessions?{r}")),
        (&f.auditor, format!("/api/v1/platform/logs/metrics?{r}")),
        (&f.owner, "/api/v1/me".into()),
        (&f.owner, "/api/v1/me/summary".into()),
        (&f.owner, "/api/v1/me/keys".into()),
    ];
    for ws in [team, personal] {
        for path in [
            format!("cost-report?{r}"),
            "cost-summary".into(),
            format!("costs?{r}"),
            format!("usage-export?{r}"),
            format!("usage/overview?{r}"),
            format!("usage/explore?{r}&metric=tokens&group_by=model"),
            format!("requests?{r}"),
            format!("generations?{r}"),
            format!("sessions?{r}"),
            format!("logs/metrics?{r}"),
            "executions".into(),
            "usage".into(),
        ] {
            out.push((&f.owner, format!("/api/v1/workspaces/{ws}/{path}")));
        }
    }
    out
}
async fn status(s: &Store, u: &BrowserPrincipal, path: &str) -> StatusCode {
    let response = routes()
        .layer(Extension(u.clone()))
        .with_state(s.clone())
        .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap());
    tokio::time::timeout(Duration::from_secs(5), response)
        .await
        .unwrap_or_else(|_| panic!("{path} waited on a lock"))
        .unwrap()
        .status()
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn reads_do_not_wait_for_held_installation_and_catalog_locks(pool: PgPool) {
    let f = fixture(&pool).await;
    let a = activity(&f, &pool).await;
    let principal = f.s.authenticate(&a.team_key).await.unwrap().unwrap();
    // The strongest holder: the exclusive catalog lock (catalog writes) plus
    // the installation row FOR UPDATE, held for the whole loop.
    let mut blocker = pool.begin().await.unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock(72419502)")
        .execute(&mut *blocker)
        .await
        .unwrap();
    sqlx::query("SELECT id FROM installation WHERE singleton FOR UPDATE")
        .execute(&mut *blocker)
        .await
        .unwrap();
    for (u, path) in reads(&f) {
        assert_eq!(status(&f.s, u, &path).await, StatusCode::OK, "{path}");
    }
    let models = tokio::time::timeout(Duration::from_secs(5), f.s.visible_models(&principal))
        .await
        .expect("/v1/models waited on a lock")
        .unwrap();
    assert_eq!(models.len(), 1);
    // Control: an installation-lock path does wait while the blocker holds.
    assert!(
        tokio::time::timeout(
            Duration::from_millis(300),
            call(
                &f.s,
                &f.owner,
                "PATCH",
                &format!("/api/v1/workspaces/{}", f.team),
                json!({"name":"Renamed"})
            )
        )
        .await
        .is_err()
    );
    blocker.rollback().await.unwrap();
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn open_snapshot_reads_do_not_block_writers_and_revocation_applies_to_the_next_read(
    pool: PgPool,
) {
    let f = fixture(&pool).await;
    let a = activity(&f, &pool).await;
    // A member's own-activity read, held open after authorizing (as a slow report would be).
    let mut reader = f.s.snapshot().await.unwrap();
    let access = resources::workspace_access_snapshot(&mut reader, &f.member, f.team)
        .await
        .unwrap();
    assert!(access.member && !access.view_all_activity);
    let platform = f.s.snapshot().await;
    let mut platform = platform.unwrap();
    resources::platform_read_snapshot(&mut platform, f.auditor.user_id)
        .await
        .unwrap();
    // Writers that used to queue behind a report: the exclusive catalog lock,
    // the installation row, and revocations of exactly the readers' access.
    let mut writer = pool.begin().await.unwrap();
    sqlx::query("SET LOCAL lock_timeout='1s'")
        .execute(&mut *writer)
        .await
        .unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock(72419502)")
        .execute(&mut *writer)
        .await
        .unwrap();
    sqlx::query("SELECT id FROM installation WHERE singleton FOR UPDATE")
        .execute(&mut *writer)
        .await
        .unwrap();
    sqlx::query("UPDATE workspace_membership_grants SET revoked_at=now() WHERE workspace_id=$1 AND user_id=$2")
        .bind(f.team).bind(f.member.user_id).execute(&mut *writer).await.unwrap();
    sqlx::query("UPDATE platform_role_grants SET revoked_at=now() WHERE user_id=$1")
        .bind(f.auditor.user_id)
        .execute(&mut *writer)
        .await
        .unwrap();
    sqlx::query("UPDATE users SET disabled_at=now() WHERE id=$1")
        .bind(f.owner.user_id)
        .execute(&mut *writer)
        .await
        .unwrap();
    writer.commit().await.unwrap();
    // Bounded staleness: the open reads keep their snapshot (still entitled
    // as of their start)...
    let still: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM effective_workspace_memberships WHERE workspace_id=$1 AND user_id=$2",
    )
    .bind(f.team)
    .bind(f.member.user_id)
    .fetch_one(&mut *reader)
    .await
    .unwrap();
    assert_eq!(still, 1);
    reader.commit().await.unwrap();
    platform.commit().await.unwrap();
    // ...and every later read sees the revocation.
    let r = range();
    for (u, path, denied) in [
        (
            &f.member,
            format!("/api/v1/workspaces/{}/usage/overview?{r}", f.team),
            StatusCode::FORBIDDEN,
        ),
        (
            &f.member,
            format!("/api/v1/workspaces/{}/requests?{r}", f.team),
            StatusCode::FORBIDDEN,
        ),
        (
            &f.auditor,
            format!("/api/v1/platform/cost-report?{r}"),
            StatusCode::FORBIDDEN,
        ),
        (
            &f.auditor,
            format!("/api/v1/platform/logs/requests?{r}"),
            StatusCode::FORBIDDEN,
        ),
        (&f.owner, "/api/v1/me".into(), StatusCode::FORBIDDEN),
        (&f.owner, "/api/v1/me/summary".into(), StatusCode::FORBIDDEN),
        (
            &f.owner,
            format!("/api/v1/workspaces/{}/usage/overview?{r}", f.personal),
            StatusCode::FORBIDDEN,
        ),
    ] {
        assert_eq!(status(&f.s, u, &path).await, denied, "{path}");
    }
    // The member's key no longer lists models (revalidation in the snapshot),
    // and inference admission still revalidates live under its locks.
    let principal = f.s.authenticate(&a.member_token).await.unwrap();
    if let Some(p) = principal {
        assert!(f.s.visible_models(&p).await.unwrap().is_empty());
        assert!(f.s.key_models(&p).await.unwrap().is_none());
    }
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn snapshot_reads_keep_personal_activity_owner_private(pool: PgPool) {
    let f = fixture(&pool).await;
    activity(&f, &pool).await;
    let r = range();
    let personal = f.personal;
    for u in [&f.admin, &f.auditor, &f.outsider, &f.member] {
        for path in [
            format!("cost-report?{r}"),
            "cost-summary".into(),
            format!("costs?{r}"),
            format!("usage-export?{r}"),
            format!("usage/overview?{r}"),
            format!("requests?{r}"),
            format!("generations?{r}"),
            format!("sessions?{r}"),
            format!("logs/metrics?{r}"),
            "executions".into(),
            "usage".into(),
        ] {
            assert_eq!(
                status(&f.s, u, &format!("/api/v1/workspaces/{personal}/{path}")).await,
                StatusCode::FORBIDDEN,
                "{path}"
            );
        }
    }
    // Platform readers see personal totals but never personal keys or requests.
    let (_, logs) = call(
        &f.s,
        &f.auditor,
        "GET",
        &format!("/api/v1/platform/logs/requests?{r}"),
        json!({}),
    )
    .await;
    assert!(
        logs["data"]
            .as_array()
            .unwrap()
            .iter()
            .all(|row| row["workspace"]["kind"] != "personal"),
        "{logs}"
    );
    let (_, explore) = call(
        &f.s,
        &f.auditor,
        "GET",
        &format!("/api/v1/platform/usage/explore?{r}&metric=requests&group_by=key"),
        json!({}),
    )
    .await;
    let rows = explore["rows"].as_array().unwrap();
    assert!(
        rows.iter()
            .any(|row| row["group"]["name"] == "Personal workspace keys"),
        "{explore}"
    );
    assert_eq!(explore["total"]["value"], "3", "{explore}");
    // Members see only their own activity in a shared workspace.
    let (_, own) = call(
        &f.s,
        &f.member,
        "GET",
        &format!("/api/v1/workspaces/{}/usage", f.team),
        json!({}),
    )
    .await;
    assert_eq!(own["requests"], "1", "{own}");
}

/// The reporting pool connects to the same database under its own
/// application name, so tests can tell which pool served a read.
async fn reporting_pool(pool: &PgPool, name: &str) -> PgPool {
    let options = (*pool.connect_options()).clone().application_name(name);
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect_with(options)
        .await
        .unwrap()
}
async fn served_by(
    tx: &mut sqlx::Transaction<'static, sqlx::Postgres>,
) -> (String, String, String) {
    sqlx::query_as("SELECT current_setting('application_name'),current_setting('transaction_isolation'),current_setting('transaction_read_only')")
        .fetch_one(&mut **tx)
        .await
        .unwrap()
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn reporting_pool_serves_report_data_and_falls_back_to_the_primary(pool: PgPool) {
    let f = fixture(&pool).await;
    activity(&f, &pool).await;
    let primary_name: String = sqlx::query_scalar("SELECT current_setting('application_name')")
        .fetch_one(&pool)
        .await
        .unwrap();
    // Snapshots are read only and REPEATABLE READ; they cannot write.
    let mut snapshot = f.s.snapshot().await.unwrap();
    assert_eq!(
        served_by(&mut snapshot).await,
        (primary_name.clone(), "repeatable read".into(), "on".into())
    );
    let denied = sqlx::query(
        "INSERT INTO audit_events(id,action,resource_type,metadata) VALUES($1,'x','x','{}')",
    )
    .bind(Uuid::new_v4())
    .execute(&mut *snapshot)
    .await
    .unwrap_err();
    assert_eq!(
        denied.as_database_error().and_then(|e| e.code()).as_deref(),
        Some("25006")
    );
    drop(snapshot);
    // No reporting pool: the authorized snapshot itself is used.
    let authorized = f.s.snapshot().await.unwrap();
    let mut tx = f.s.reporting(authorized).await.unwrap();
    assert_eq!(served_by(&mut tx).await.0, primary_name);
    tx.commit().await.unwrap();

    // A healthy reporting pool serves the data snapshot.
    let replica = f.s.clone().with_reporting(
        Some(reporting_pool(&pool, "omg_reporting_test").await),
        Duration::from_secs(30),
    );
    let authorized = replica.snapshot().await.unwrap();
    let mut tx = replica.reporting(authorized).await.unwrap();
    assert_eq!(
        served_by(&mut tx).await,
        (
            "omg_reporting_test".into(),
            "repeatable read".into(),
            "on".into()
        )
    );
    tx.commit().await.unwrap();
    for (u, path) in reads(&f) {
        assert_eq!(status(&replica, u, &path).await, StatusCode::OK, "{path}");
    }
    // Authorization still happens on the primary: a revoked membership is
    // denied even though the reporting pool would serve the rows.
    sqlx::query("UPDATE workspace_membership_grants SET revoked_at=now() WHERE workspace_id=$1 AND user_id=$2")
        .bind(f.team).bind(f.member.user_id).execute(&pool).await.unwrap();
    assert_eq!(
        status(
            &replica,
            &f.member,
            &format!("/api/v1/workspaces/{}/usage/overview?{}", f.team, range())
        )
        .await,
        StatusCode::FORBIDDEN
    );

    // An unreachable replica falls back to the primary snapshot.
    let unreachable = sqlx::postgres::PgPoolOptions::new()
        .acquire_timeout(Duration::from_millis(500))
        .connect_lazy("postgres://nobody:nothing@127.0.0.1:1/none")
        .unwrap();
    let broken =
        f.s.clone()
            .with_reporting(Some(unreachable), Duration::from_secs(30));
    let authorized = broken.snapshot().await.unwrap();
    let mut tx = broken.reporting(authorized).await.unwrap();
    assert_eq!(served_by(&mut tx).await.0, primary_name);
    tx.commit().await.unwrap();
    for (u, path) in reads(&f) {
        assert_eq!(status(&broken, u, &path).await, StatusCode::OK, "{path}");
    }
}

/// Drive `fut` until it has returned `Pending` `pendings + 1` times, then
/// drop it (a client disconnect or handler deadline at that await point).
/// `true` when it completed first.
async fn cancel_after<F: std::future::Future>(fut: F, pendings: usize) -> bool {
    let mut fut = std::pin::pin!(fut);
    let mut seen = 0;
    std::future::poll_fn(|cx| match fut.as_mut().poll(cx) {
        std::task::Poll::Ready(_) => std::task::Poll::Ready(true),
        std::task::Poll::Pending if seen == pendings => std::task::Poll::Ready(false),
        std::task::Poll::Pending => {
            seen += 1;
            std::task::Poll::Pending
        }
    })
    .await
}

/// Regression (browser journey 503 "Management storage unavailable", SQLSTATE
/// 25001): a transaction begin dropped after `BEGIN` reached the server left
/// the pooled connection inside a server-side transaction that sqlx did not
/// track. Plain pool queries then ran inside it (taking its snapshot) and the
/// next lock-free snapshot failed with "SET TRANSACTION ISOLATION LEVEL must
/// be called before any query". Every cancellation point of every begin path
/// must leave the connection clean.
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn cancelled_transaction_begins_never_leak_into_the_next_snapshot(pool: PgPool) {
    // One connection, so the next request reuses the cancelled one.
    let one = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect_with((*pool.connect_options()).clone())
        .await
        .unwrap();
    let s = Store::new(one.clone());
    for path in ["snapshot", "installation"] {
        for pendings in 0.. {
            let completed = match path {
                "snapshot" => cancel_after(s.snapshot(), pendings).await,
                _ => cancel_after(resources::installation_tx(&s), pendings).await,
            };
            // Let a begin that outlived its caller finish and release the
            // connection first (the pool queues waiters in order).
            sqlx::query("SELECT 1").execute(&one).await.unwrap();
            // A plain pool statement (session lookup, audit, ...) on the same
            // connection must run outside any transaction.
            let reused: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
                .fetch_one(&one)
                .await
                .unwrap();
            let state: Option<String> =
                sqlx::query_scalar("SELECT state FROM pg_stat_activity WHERE pid=$1")
                    .bind(reused)
                    .fetch_optional(&pool)
                    .await
                    .unwrap();
            let mut tx = s
                .snapshot()
                .await
                .unwrap_or_else(|e| panic!("{path} cancelled after {pendings} polls: {e}"));
            assert!(
                state.as_deref() != Some("idle in transaction"),
                "{path} cancelled after {pendings} polls left the connection {state:?}"
            );
            let (_, isolation, read_only) = served_by(&mut tx).await;
            tx.commit().await.unwrap();
            assert_eq!(
                (isolation.as_str(), read_only.as_str()),
                ("repeatable read", "on")
            );
            if completed {
                break;
            }
        }
    }
}

/// Captures formatted log lines for one test.
#[derive(Clone, Default)]
struct Captured(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
impl std::io::Write for Captured {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Captured {
    type Writer = Captured;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// A database error mapped to 503 "Management storage unavailable" is logged
/// with the request id, SQLSTATE and object names; the PostgreSQL message only
/// for classes that never interpolate values, and never row data (DETAIL).
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn storage_errors_are_logged_sanitized_with_the_request_id(pool: PgPool) {
    let s = Store::new(pool.clone());
    // 25006 inside a read-only snapshot: message kept.
    let mut tx = s.snapshot().await.unwrap();
    let read_only = sqlx::query("INSERT INTO users(id,email) VALUES($1,'secret-row@example.test')")
        .bind(Uuid::new_v4())
        .execute(&mut *tx)
        .await
        .unwrap_err();
    drop(tx);
    let f = storage_error_fields(&read_only);
    assert_eq!(f.kind, "database");
    assert_eq!(f.sqlstate.as_deref(), Some("25006"));
    assert_eq!(
        f.message.as_deref(),
        Some("cannot execute INSERT in a read-only transaction")
    );
    // 22P02 echoes the bound value in its message: dropped.
    let data = sqlx::query("SELECT $1::text::uuid")
        .bind("secret-value")
        .execute(&pool)
        .await
        .unwrap_err();
    let f = storage_error_fields(&data);
    assert_eq!(f.sqlstate.as_deref(), Some("22P02"));
    assert_eq!(f.message, None);
    // A not-null violation names its table/column but logs no row values.
    let integrity =
        sqlx::query("INSERT INTO users(id,email) VALUES(NULL,'secret-row@example.test')")
            .execute(&pool)
            .await
            .unwrap_err();
    let f = storage_error_fields(&integrity);
    assert_eq!(f.sqlstate.as_deref(), Some("23502"));
    assert_eq!(
        (f.table.as_deref(), f.column.as_deref()),
        (Some("users"), Some("id"))
    );
    assert_eq!(f.message, None);
    assert_eq!(
        storage_error_fields(&sqlx::Error::PoolTimedOut).kind,
        "pool_timed_out"
    );

    let out = Captured::default();
    let subscriber = tracing_subscriber::fmt()
        .json()
        .with_writer(out.clone())
        .finish();
    let statuses = tracing::subscriber::with_default(subscriber, || {
        let span = tracing::info_span!("http.request", request_id = "req-7f3a", method = "GET");
        let _entered = span.enter();
        [read_only, data, integrity].map(|e| ApiError::from(e).0)
    });
    assert_eq!(statuses, [StatusCode::SERVICE_UNAVAILABLE; 3]);
    let logged = String::from_utf8(out.0.lock().unwrap().clone()).unwrap();
    let lines: Vec<Value> = logged
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines.len(), 3, "{logged}");
    for (line, code) in lines.iter().zip(["25006", "22P02", "23502"]) {
        assert_eq!(line["fields"]["message"], "management storage error");
        assert_eq!(line["fields"]["sqlstate"], code);
        assert_eq!(line["span"]["request_id"], "req-7f3a");
    }
    assert_eq!(
        lines[0]["fields"]["db_message"],
        "cannot execute INSERT in a read-only transaction"
    );
    for secret in ["secret-value", "secret-row", "Failing row"] {
        assert!(!logged.contains(secret), "{logged}");
    }
}
