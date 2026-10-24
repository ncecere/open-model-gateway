//! #253 slice A: bounded key issuance, on a real PostgreSQL database.
//!
//! Seeding writes `api_keys` rows through the owner pool at chosen ages; every
//! enforced path goes through the served handlers.
use super::*;
use axum::http::HeaderMap;

const DAY: i64 = 86_400;

async fn call_h(
    s: &Store,
    u: &BrowserPrincipal,
    method: &str,
    path: &str,
    body: Value,
) -> (StatusCode, HeaderMap, Value) {
    let response = routes()
        .layer(Extension(ManagementPrincipal::Browser(u.clone())))
        .layer(Extension(u.clone()))
        .with_state(s.clone())
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
    let headers = response.headers().clone();
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    (
        status,
        headers,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}
async fn create(f: &Fixture, u: &BrowserPrincipal, ws: Uuid) -> (StatusCode, HeaderMap, Value) {
    call_h(
        &f.s,
        u,
        "POST",
        &format!("/api/v1/workspaces/{ws}/keys"),
        json!({"name":"Issued","expires_in_days":1}),
    )
    .await
}
async fn create_for(
    f: &Fixture,
    u: &BrowserPrincipal,
    ws: Uuid,
    account: Uuid,
) -> (StatusCode, HeaderMap, Value) {
    call_h(
        &f.s,
        u,
        "POST",
        &format!("/api/v1/workspaces/{ws}/keys"),
        json!({"name":"Robot key","expires_in_days":1,"service_account_id":account}),
    )
    .await
}
async fn rotate(
    f: &Fixture,
    u: &BrowserPrincipal,
    ws: Uuid,
    key: Uuid,
) -> (StatusCode, HeaderMap, Value) {
    call_h(
        &f.s,
        u,
        "POST",
        &format!("/api/v1/workspaces/{ws}/keys/{key}/rotate"),
        json!({"expires_in_days":1}),
    )
    .await
}
/// `n` key rows of `holder`, created `age_secs` ago (owner pool: the runtime
/// never chooses `created_at`).
async fn seed(pool: &PgPool, ws: Uuid, user: Uuid, n: i64, age_secs: i64) -> Vec<Uuid> {
    sqlx::query_scalar("INSERT INTO api_keys(id,workspace_id,issued_to_user_id,name,secret_hash,expires_at,created_at) SELECT gen_random_uuid(),$1,$2,'Seed '||g,decode(md5(random()::text||g::text)||md5(g::text||random()::text),'hex'),now()+interval '1 day',now()-make_interval(secs=>$4) FROM generate_series(1,$3) g RETURNING id")
        .bind(ws).bind(user).bind(n).bind(age_secs as f64)
        .fetch_all(pool).await.unwrap()
}
async fn type_limits(pool: &PgPool, kind: &str, holder: Option<i64>, workspace: Option<i64>) {
    sqlx::query("UPDATE workspace_type_policies SET key_issuances_per_holder=$2,key_issuances_per_workspace=$3 WHERE kind=$1")
        .bind(kind).bind(holder).bind(workspace).execute(pool).await.unwrap();
}
async fn keys_of(pool: &PgPool, ws: Uuid) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM api_keys WHERE workspace_id=$1")
        .bind(ws)
        .fetch_one(pool)
        .await
        .unwrap()
}
async fn audits(pool: &PgPool, action: &str) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM audit_events WHERE action=$1")
        .bind(action)
        .fetch_one(pool)
        .await
        .unwrap()
}
fn policy(extra: Value) -> Value {
    let mut v = json!({"requests_per_minute":null,"tokens_per_minute":null,"concurrent_requests":null,"budgets":[]});
    v.as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    v
}
async fn put(f: &Fixture, u: &BrowserPrincipal, path: &str, body: Value) -> (StatusCode, Value) {
    let (s, _, v) = call_h(&f.s, u, "PUT", path, body).await;
    (s, v)
}
fn reason(v: &Value) -> &str {
    v["error"]["reason"].as_str().unwrap_or("")
}
/// Runs the requests at once, each on its own connection.
async fn race(
    f: &Fixture,
    jobs: Vec<(BrowserPrincipal, String, String, Value)>,
) -> Vec<StatusCode> {
    let tasks: Vec<_> = jobs
        .into_iter()
        .map(|(u, method, path, body)| {
            let s = f.s.clone();
            tokio::spawn(async move { call_h(&s, &u, &method, &path, body).await.0 })
        })
        .collect();
    let mut out = Vec::new();
    for t in tasks {
        out.push(t.await.unwrap());
    }
    out
}
fn creates(
    u: &BrowserPrincipal,
    ws: Uuid,
    n: usize,
) -> Vec<(BrowserPrincipal, String, String, Value)> {
    (0..n)
        .map(|i| {
            (
                u.clone(),
                "POST".to_owned(),
                format!("/api/v1/workspaces/{ws}/keys"),
                json!({"name":format!("Race {i}"),"expires_in_days":1}),
            )
        })
        .collect()
}
fn rotations(
    u: &BrowserPrincipal,
    ws: Uuid,
    keys: &[Uuid],
) -> Vec<(BrowserPrincipal, String, String, Value)> {
    keys.iter()
        .map(|k| {
            (
                u.clone(),
                "POST".to_owned(),
                format!("/api/v1/workspaces/{ws}/keys/{k}/rotate"),
                json!({"expires_in_days":1}),
            )
        })
        .collect()
}
fn count(codes: &[StatusCode], status: StatusCode) -> usize {
    codes.iter().filter(|c| **c == status).count()
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn type_defaults_are_50_per_holder_and_500_per_workspace_for_every_kind(pool: PgPool) {
    for kind in ["personal", "team", "project"] {
        let row: (Option<i64>, Option<i64>) = sqlx::query_as("SELECT key_issuances_per_holder,key_issuances_per_workspace FROM workspace_type_policies WHERE kind=$1")
            .bind(kind).fetch_one(&pool).await.unwrap();
        assert_eq!(row, (Some(50), Some(500)), "{kind}");
    }
    for table in [
        "workspace_platform_policy_overrides",
        "workspace_local_policies",
    ] {
        let defaults: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM information_schema.columns WHERE table_name=$1 AND column_name LIKE 'key_issuances_%' AND column_default IS NOT NULL",
        )
        .bind(table)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(defaults, 0, "{table} columns inherit (NULL)");
    }
    let index: i64 = sqlx::query_scalar("SELECT count(*) FROM pg_indexes WHERE indexname='api_keys_workspace_created' AND indexdef LIKE '%(workspace_id, created_at)%'")
        .fetch_one(&pool).await.unwrap();
    assert_eq!(index, 1);
    // The column ceiling and floor are 10000 and 1.
    for bad in [0_i64, 10_001] {
        assert!(
            sqlx::query(
                "UPDATE workspace_type_policies SET key_issuances_per_holder=$1 WHERE kind='team'"
            )
            .bind(bad)
            .execute(&pool)
            .await
            .is_err(),
            "{bad}"
        );
    }
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn the_default_holder_budget_admits_50_and_refuses_the_51st_with_429(pool: PgPool) {
    let f = fixture(&pool).await;
    seed(&pool, f.team, f.member.user_id, 49, 60).await;
    let (status, _, v) = create(&f, &f.member, f.team).await;
    assert_eq!(status, StatusCode::OK, "{v}");
    let (status, headers, v) = create(&f, &f.member, f.team).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{v}");
    assert_eq!(reason(&v), "key_issuance_limit_exceeded");
    assert_eq!(keys_of(&pool, f.team).await, 50);
    // The oldest counted row (60 s old) ages out in just under a day.
    let retry = v["error"]["retry_after_seconds"].as_i64().unwrap();
    assert!((DAY - 62..=DAY - 58).contains(&retry), "{retry}");
    assert_eq!(
        headers.get("retry-after").unwrap().to_str().unwrap(),
        retry.to_string()
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn refusal_is_a_detailed_429_without_amounts_audit_or_rows(pool: PgPool) {
    let f = fixture(&pool).await;
    type_limits(&pool, "team", Some(2), Some(500)).await;
    for _ in 0..2 {
        assert_eq!(create(&f, &f.member, f.team).await.0, StatusCode::OK);
    }
    let audit_before = audits(&pool, "key.created").await;
    let (status, headers, v) = create(&f, &f.member, f.team).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    let error = v["error"].as_object().unwrap();
    let mut names: Vec<_> = error.keys().map(String::as_str).collect();
    names.sort_unstable();
    assert_eq!(
        names,
        [
            "code",
            "message",
            "operation",
            "reason",
            "retry_after_seconds",
            "scope",
            "window_seconds"
        ],
        "{v}"
    );
    assert_eq!(error["code"], "429");
    assert_eq!(error["reason"], "key_issuance_limit_exceeded");
    assert_eq!(error["scope"], "holder");
    assert_eq!(error["operation"], "create");
    assert_eq!(error["window_seconds"], DAY);
    assert!(error["retry_after_seconds"].as_i64().unwrap() > 0);
    assert!(headers.get("retry-after").is_some());
    // The body names no limit, count, other holder or workspace.
    let text = v.to_string();
    for forbidden in [
        f.owner.user_id.to_string(),
        f.team.to_string(),
        f.member.email.clone(),
    ] {
        assert!(!text.contains(&forbidden), "{text}");
    }
    assert_eq!(keys_of(&pool, f.team).await, 2, "refusal left a row");
    assert_eq!(
        audits(&pool, "key.created").await,
        audit_before,
        "refusals are not audited"
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM audit_events WHERE action LIKE '%issuance%' OR action LIKE '%refus%'")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn parallel_creates_at_limit_minus_one_admit_exactly_one(pool: PgPool) {
    let f = fixture(&pool).await;
    type_limits(&pool, "team", Some(5), Some(500)).await;
    seed(&pool, f.team, f.member.user_id, 4, 60).await;
    let codes = race(&f, creates(&f.member, f.team, 6)).await;
    assert_eq!(count(&codes, StatusCode::OK), 1, "{codes:?}");
    assert_eq!(count(&codes, StatusCode::TOO_MANY_REQUESTS), 5, "{codes:?}");
    assert_eq!(keys_of(&pool, f.team).await, 5);
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn parallel_rotations_at_limit_minus_one_admit_exactly_one(pool: PgPool) {
    let f = fixture(&pool).await;
    type_limits(&pool, "team", Some(5), Some(500)).await;
    let keys = seed(&pool, f.team, f.member.user_id, 4, 60).await;
    let codes = race(&f, rotations(&f.member, f.team, &keys)).await;
    assert_eq!(count(&codes, StatusCode::OK), 1, "{codes:?}");
    assert_eq!(count(&codes, StatusCode::TOO_MANY_REQUESTS), 3, "{codes:?}");
    assert_eq!(keys_of(&pool, f.team).await, 5);
    // A refused rotation changes nothing: its key is still the live one.
    let revoked: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM api_keys WHERE workspace_id=$1 AND revoked_at IS NOT NULL",
    )
    .bind(f.team)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(revoked, 1);
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn parallel_create_and_rotate_mix_admits_exactly_one(pool: PgPool) {
    let f = fixture(&pool).await;
    type_limits(&pool, "team", Some(5), Some(500)).await;
    let keys = seed(&pool, f.team, f.member.user_id, 4, 60).await;
    let mut jobs = creates(&f.member, f.team, 2);
    jobs.extend(rotations(&f.member, f.team, &keys[..2]));
    let codes = race(&f, jobs).await;
    assert_eq!(count(&codes, StatusCode::OK), 1, "{codes:?}");
    assert_eq!(count(&codes, StatusCode::TOO_MANY_REQUESTS), 3, "{codes:?}");
    assert_eq!(keys_of(&pool, f.team).await, 5);
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn a_rotation_reports_its_operation_and_counts_like_a_create(pool: PgPool) {
    let f = fixture(&pool).await;
    type_limits(&pool, "team", Some(2), Some(500)).await;
    let (status, _, v) = create(&f, &f.member, f.team).await;
    assert_eq!(status, StatusCode::OK, "{v}");
    let (status, _, v) = rotate(&f, &f.member, f.team, id(&v)).await;
    assert_eq!(status, StatusCode::OK, "{v}");
    let (status, _, v) = rotate(&f, &f.member, f.team, id(&v)).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{v}");
    assert_eq!(v["error"]["operation"], "rotate");
    assert_eq!(v["error"]["scope"], "holder");
    assert_eq!(
        create(&f, &f.member, f.team).await.0,
        StatusCode::TOO_MANY_REQUESTS
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn holder_at_its_cap_does_not_block_another_holder(pool: PgPool) {
    let f = fixture(&pool).await;
    type_limits(&pool, "team", Some(2), Some(500)).await;
    seed(&pool, f.team, f.member.user_id, 2, 60).await;
    assert_eq!(
        create(&f, &f.member, f.team).await.0,
        StatusCode::TOO_MANY_REQUESTS
    );
    assert_eq!(create(&f, &f.owner, f.team).await.0, StatusCode::OK);
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn workspace_cap_binds_every_holder_and_holder_is_checked_first(pool: PgPool) {
    let f = fixture(&pool).await;
    type_limits(&pool, "team", Some(10), Some(3)).await;
    seed(&pool, f.team, f.owner.user_id, 2, 60).await;
    assert_eq!(create(&f, &f.member, f.team).await.0, StatusCode::OK);
    // Workspace budget exhausted by other holders: this holder is far below its own.
    let (status, _, v) = create(&f, &f.member, f.team).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{v}");
    assert_eq!(v["error"]["scope"], "workspace");
    assert_eq!(keys_of(&pool, f.team).await, 3);
    // With both budgets exhausted the holder scope is reported.
    type_limits(&pool, "team", Some(1), Some(3)).await;
    let (_, _, v) = create(&f, &f.member, f.team).await;
    assert_eq!(v["error"]["scope"], "holder");
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn parallel_creates_by_different_holders_respect_the_workspace_cap(pool: PgPool) {
    let f = fixture(&pool).await;
    type_limits(&pool, "team", Some(100), Some(5)).await;
    seed(&pool, f.team, f.member.user_id, 2, 60).await;
    seed(&pool, f.team, f.owner.user_id, 2, 60).await;
    let mut jobs = creates(&f.member, f.team, 3);
    jobs.extend(creates(&f.owner, f.team, 3));
    let codes = race(&f, jobs).await;
    assert_eq!(count(&codes, StatusCode::OK), 1, "{codes:?}");
    assert_eq!(count(&codes, StatusCode::TOO_MANY_REQUESTS), 5, "{codes:?}");
    assert_eq!(keys_of(&pool, f.team).await, 5);
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn workspaces_are_budgeted_independently(pool: PgPool) {
    let f = fixture(&pool).await;
    type_limits(&pool, "team", Some(1), Some(500)).await;
    type_limits(&pool, "project", Some(1), Some(500)).await;
    seed(&pool, f.team, f.owner.user_id, 1, 60).await;
    assert_eq!(
        create(&f, &f.owner, f.team).await.0,
        StatusCode::TOO_MANY_REQUESTS
    );
    assert_eq!(create(&f, &f.owner, f.project).await.0, StatusCode::OK);
    // The type default is per kind: personal keeps its own.
    assert_eq!(create(&f, &f.owner, f.personal).await.0, StatusCode::OK);
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn the_window_rolls_at_24_hours(pool: PgPool) {
    let f = fixture(&pool).await;
    type_limits(&pool, "team", Some(1), Some(500)).await;
    // Just outside the window: not counted.
    seed(&pool, f.team, f.member.user_id, 1, DAY + 1).await;
    // Just inside it (the margin covers the test's own latency): counted.
    seed(&pool, f.team, f.owner.user_id, 1, DAY - 3).await;
    assert_eq!(create(&f, &f.member, f.team).await.0, StatusCode::OK);
    let (status, headers, v) = create(&f, &f.owner, f.team).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{v}");
    let retry = v["error"]["retry_after_seconds"].as_i64().unwrap();
    assert!((1..=3).contains(&retry), "{retry}");
    assert_eq!(
        headers.get("retry-after").unwrap().to_str().unwrap(),
        retry.to_string()
    );
    tokio::time::sleep(std::time::Duration::from_secs(retry as u64 + 1)).await;
    assert_eq!(create(&f, &f.owner, f.team).await.0, StatusCode::OK);
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn retry_after_names_the_row_that_must_age_out(pool: PgPool) {
    let f = fixture(&pool).await;
    type_limits(&pool, "team", Some(2), Some(500)).await;
    // Two counted rows: refusing a third needs only the older one to expire.
    seed(&pool, f.team, f.member.user_id, 1, 7_200).await;
    seed(&pool, f.team, f.member.user_id, 1, 3_600).await;
    let (_, _, v) = create(&f, &f.member, f.team).await;
    let retry = v["error"]["retry_after_seconds"].as_i64().unwrap();
    assert!((DAY - 7_202..=DAY - 7_198).contains(&retry), "{retry}");
    // Over the limit by two (limit lowered): both of the older rows must expire.
    type_limits(&pool, "team", Some(1), Some(500)).await;
    let (_, _, v) = create(&f, &f.member, f.team).await;
    let retry = v["error"]["retry_after_seconds"].as_i64().unwrap();
    assert!((DAY - 3_602..=DAY - 3_598).contains(&retry), "{retry}");
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn raising_admits_at_once_and_lowering_never_revokes(pool: PgPool) {
    let f = fixture(&pool).await;
    let path = "/api/v1/platform/workspace-types/team/policy";
    let (status, v) = put(
        &f,
        &f.admin,
        path,
        policy(json!({"key_issuances_per_holder":2})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{v}");
    let mut tokens = Vec::new();
    for _ in 0..2 {
        let (status, _, v) = create(&f, &f.member, f.team).await;
        assert_eq!(status, StatusCode::OK, "{v}");
        tokens.push(v["token"].as_str().unwrap().to_owned());
    }
    assert_eq!(
        create(&f, &f.member, f.team).await.0,
        StatusCode::TOO_MANY_REQUESTS
    );
    // Raising admits the very next request; nothing is reset.
    let (status, _) = put(
        &f,
        &f.admin,
        path,
        policy(json!({"key_issuances_per_holder":3})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(create(&f, &f.member, f.team).await.0, StatusCode::OK);
    assert_eq!(
        create(&f, &f.member, f.team).await.0,
        StatusCode::TOO_MANY_REQUESTS
    );
    // Lowering below the use refuses new keys and leaves existing ones alone.
    let (status, _) = put(
        &f,
        &f.admin,
        path,
        policy(json!({"key_issuances_per_holder":1})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        create(&f, &f.member, f.team).await.0,
        StatusCode::TOO_MANY_REQUESTS
    );
    for token in &tokens {
        assert!(f.s.authenticate(token).await.unwrap().is_some());
    }
    // Absent keeps the stored value; null clears it to the 10000 ceiling.
    let (status, _) = put(&f, &f.admin, path, policy(json!({}))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        create(&f, &f.member, f.team).await.0,
        StatusCode::TOO_MANY_REQUESTS
    );
    let (status, _) = put(
        &f,
        &f.admin,
        path,
        policy(json!({"key_issuances_per_holder":null})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(create(&f, &f.member, f.team).await.0, StatusCode::OK);
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn a_platform_override_replaces_the_type_default(pool: PgPool) {
    let f = fixture(&pool).await;
    type_limits(&pool, "team", Some(1), Some(500)).await;
    seed(&pool, f.team, f.member.user_id, 1, 60).await;
    assert_eq!(
        create(&f, &f.member, f.team).await.0,
        StatusCode::TOO_MANY_REQUESTS
    );
    let path = format!("/api/v1/platform/workspaces/{}/policy", f.team);
    let (status, v) = put(
        &f,
        &f.admin,
        &path,
        policy(json!({"key_issuances_per_holder":3,"key_issuances_per_workspace":4})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{v}");
    assert_eq!(create(&f, &f.member, f.team).await.0, StatusCode::OK);
    assert_eq!(create(&f, &f.member, f.team).await.0, StatusCode::OK);
    let (_, _, v) = create(&f, &f.member, f.team).await;
    assert_eq!(v["error"]["scope"], "holder");
    // The workspace budget of the override binds other holders too.
    assert_eq!(create(&f, &f.owner, f.team).await.0, StatusCode::OK);
    let (_, _, v) = create(&f, &f.owner, f.team).await;
    assert_eq!(v["error"]["scope"], "workspace");
    // Resetting the override restores the type default (1 per holder).
    let (status, _, _) = call_h(&f.s, &f.admin, "DELETE", &path, Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    type_limits(&pool, "team", Some(100), Some(500)).await;
    assert_eq!(create(&f, &f.member, f.team).await.0, StatusCode::OK);
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn local_limits_are_tighten_only_and_compose_with_the_platform(pool: PgPool) {
    let f = fixture(&pool).await;
    type_limits(&pool, "team", Some(5), Some(500)).await;
    let path = format!("/api/v1/workspaces/{}/policy", f.team);
    // Above the platform layer: 400 exceeds_parent_rate, naming only the limit.
    let (status, v) = put(
        &f,
        &f.owner,
        &path,
        policy(json!({"key_issuances_per_holder":6})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{v}");
    assert_eq!(reason(&v), "exceeds_parent_rate");
    assert_eq!(v["error"]["limit"], "key_issuances_per_holder");
    let (status, v) = put(
        &f,
        &f.owner,
        &path,
        policy(json!({"key_issuances_per_workspace":501})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{v}");
    assert_eq!(v["error"]["limit"], "key_issuances_per_workspace");
    // Below it: stored, and the lower of the two limits applies.
    let (status, v) = put(
        &f,
        &f.owner,
        &path,
        policy(json!({"key_issuances_per_holder":2,"key_issuances_per_workspace":3})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{v}");
    let (_, _, v) = call_h(&f.s, &f.owner, "GET", &path, Value::Null).await;
    assert_eq!(v["policy"]["key_issuances_per_holder"], 2);
    assert_eq!(v["effective"]["key_issuances_per_holder"], 2);
    assert_eq!(v["effective"]["key_issuances_per_workspace"], 3);
    assert_eq!(v["provenance"]["platform"]["key_issuances_per_holder"], 5);
    assert_eq!(create(&f, &f.member, f.team).await.0, StatusCode::OK);
    assert_eq!(create(&f, &f.member, f.team).await.0, StatusCode::OK);
    let (_, _, v) = create(&f, &f.member, f.team).await;
    assert_eq!(v["error"]["scope"], "holder");
    assert_eq!(create(&f, &f.owner, f.team).await.0, StatusCode::OK);
    let (_, _, v) = create(&f, &f.owner, f.team).await;
    assert_eq!(v["error"]["scope"], "workspace");
    // A stored local cap can neither be raised nor removed.
    for body in [
        json!({"key_issuances_per_holder":3,"key_issuances_per_workspace":3}),
        json!({"key_issuances_per_holder":null,"key_issuances_per_workspace":3}),
    ] {
        let (status, v) = put(&f, &f.owner, &path, policy(body)).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{v}");
        assert_eq!(reason(&v), "stored_rate_loosen_not_allowed");
        assert_eq!(v["error"]["limit"], "key_issuances_per_holder");
    }
    // Absent keeps the stored local cap.
    let (status, _) = put(&f, &f.owner, &path, policy(json!({}))).await;
    assert_eq!(status, StatusCode::OK);
    let (_, _, v) = call_h(&f.s, &f.owner, "GET", &path, Value::Null).await;
    assert_eq!(v["policy"]["key_issuances_per_workspace"], 3);
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn limits_are_validated_and_keys_have_no_issuance_layer(pool: PgPool) {
    let f = fixture(&pool).await;
    let type_path = "/api/v1/platform/workspace-types/team/policy";
    for bad in [json!(0), json!(-1), json!(10_001), json!("5"), json!(1.5)] {
        let (status, _) = put(
            &f,
            &f.admin,
            type_path,
            policy(json!({"key_issuances_per_holder":bad})),
        )
        .await;
        assert!(status.is_client_error(), "{bad}");
        let (status, _) = put(
            &f,
            &f.admin,
            type_path,
            policy(json!({"key_issuances_per_workspace":bad})),
        )
        .await;
        assert!(status.is_client_error(), "{bad}");
    }
    let (status, v) = put(
        &f,
        &f.admin,
        type_path,
        policy(json!({"key_issuances_per_holder":10_000,"key_issuances_per_workspace":1})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{v}");
    let (_, _, v) = call_h(&f.s, &f.admin, "GET", type_path, Value::Null).await;
    assert_eq!(v["policy"]["key_issuances_per_holder"], 10_000);
    assert_eq!(v["policy"]["key_issuances_per_workspace"], 1);
    // No key layer: a key policy cannot carry them (as for storage_bytes).
    let (_, _, k) = create(&f, &f.member, f.team).await;
    let key_path = format!("/api/v1/workspaces/{}/keys/{}/policy", f.team, id(&k));
    let (status, _) = put(
        &f,
        &f.member,
        &key_path,
        policy(json!({"key_issuances_per_holder":1})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = put(
        &f,
        &f.member,
        &key_path,
        policy(json!({"key_issuances_per_workspace":1})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = put(
        &f,
        &f.member,
        &key_path,
        policy(json!({"key_issuances_per_holder":null})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (_, _, v) = call_h(&f.s, &f.member, "GET", &key_path, Value::Null).await;
    assert_eq!(v["policy"]["key_issuances_per_holder"], Value::Null);
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn a_missing_platform_value_means_the_10000_ceiling_not_unlimited(pool: PgPool) {
    let f = fixture(&pool).await;
    type_limits(&pool, "team", None, None).await;
    seed(&pool, f.team, f.owner.user_id, 9_999, 60).await;
    assert_eq!(create(&f, &f.owner, f.team).await.0, StatusCode::OK);
    let (status, _, v) = create(&f, &f.owner, f.team).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{v}");
    // The holder budget is exhausted at the ceiling; so is the workspace's.
    let (_, _, v) = create(&f, &f.member, f.team).await;
    assert_eq!(v["error"]["scope"], "workspace");
    // An override row with a NULL value is the same ceiling, not the type default.
    sqlx::query("INSERT INTO workspace_platform_policy_overrides(workspace_id) VALUES($1)")
        .bind(f.project)
        .execute(&pool)
        .await
        .unwrap();
    type_limits(&pool, "project", Some(1), Some(1)).await;
    seed(&pool, f.project, f.owner.user_id, 5, 60).await;
    assert_eq!(create(&f, &f.owner, f.project).await.0, StatusCode::OK);
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn personal_workspaces_use_the_type_default_only(pool: PgPool) {
    let f = fixture(&pool).await;
    type_limits(&pool, "personal", Some(2), Some(500)).await;
    assert_eq!(create(&f, &f.owner, f.personal).await.0, StatusCode::OK);
    assert_eq!(create(&f, &f.owner, f.personal).await.0, StatusCode::OK);
    let (status, _, v) = create(&f, &f.owner, f.personal).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{v}");
    assert_eq!(v["error"]["scope"], "holder");
    // The owner cannot tighten or loosen it: personal limits stay platform-controlled.
    let (status, v) = put(
        &f,
        &f.owner,
        &format!("/api/v1/workspaces/{}/policy", f.personal),
        policy(json!({"key_issuances_per_holder":1})),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{v}");
    assert_eq!(reason(&v), "personal_limits_platform_controlled");
    // A platform override applies to the personal workspace.
    let (status, v) = put(
        &f,
        &f.admin,
        &format!("/api/v1/platform/workspaces/{}/policy", f.personal),
        policy(json!({"key_issuances_per_holder":3})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{v}");
    assert_eq!(create(&f, &f.owner, f.personal).await.0, StatusCode::OK);
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn service_account_keys_count_against_their_account_and_the_workspace(pool: PgPool) {
    let f = fixture(&pool).await;
    type_limits(&pool, "team", Some(2), Some(5)).await;
    let mut accounts = Vec::new();
    for name in ["Robot one", "Robot two", "Robot three"] {
        let (status, v) = call(
            &f.s,
            &f.owner,
            "POST",
            &format!("/api/v1/workspaces/{}/service-accounts", f.team),
            json!({ "name": name }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{v}");
        accounts.push(id(&v));
    }
    let (one, two, three) = (accounts[0], accounts[1], accounts[2]);
    let (status, _, v) = create_for(&f, &f.owner, f.team, one).await;
    assert_eq!(status, StatusCode::OK, "{v}");
    // Rotation counts against the account too.
    let (status, _, v) = rotate(&f, &f.owner, f.team, id(&v)).await;
    assert_eq!(status, StatusCode::OK, "{v}");
    let (status, _, v) = create_for(&f, &f.owner, f.team, one).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{v}");
    assert_eq!(v["error"]["scope"], "holder");
    // The creating admin is not capped by the account's keys.
    assert_eq!(create(&f, &f.owner, f.team).await.0, StatusCode::OK);
    assert_eq!(create(&f, &f.owner, f.team).await.0, StatusCode::OK);
    // Another account has its own holder budget, but the workspace's is shared.
    assert_eq!(
        create_for(&f, &f.owner, f.team, two).await.0,
        StatusCode::OK
    );
    let (status, _, v) = create_for(&f, &f.owner, f.team, three).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{v}");
    assert_eq!(v["error"]["scope"], "workspace");
    assert_eq!(keys_of(&pool, f.team).await, 5);
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn service_account_churn_cannot_exceed_the_workspace_budget(pool: PgPool) {
    let f = fixture(&pool).await;
    type_limits(&pool, "team", Some(100), Some(3)).await;
    for i in 0..5 {
        let (status, v) = call(
            &f.s,
            &f.owner,
            "POST",
            &format!("/api/v1/workspaces/{}/service-accounts", f.team),
            json!({"name":format!("Churn {i}")}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{v}");
        let account = id(&v);
        let (status, _, body) = create_for(&f, &f.owner, f.team, account).await;
        if i < 3 {
            assert_eq!(status, StatusCode::OK, "{body}");
        } else {
            assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
        }
        // Disabling an account revokes its keys; the rows still count.
        let (status, _) = call(
            &f.s,
            &f.owner,
            "PATCH",
            &format!("/api/v1/workspaces/{}/service-accounts/{account}", f.team),
            json!({"disabled":true}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }
    assert_eq!(keys_of(&pool, f.team).await, 3);
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn authority_and_validation_errors_precede_the_429(pool: PgPool) {
    let f = fixture(&pool).await;
    type_limits(&pool, "team", Some(1), Some(1)).await;
    let keys = seed(&pool, f.team, f.member.user_id, 1, 60).await;
    // Non-members and unknown workspaces learn nothing about the budget.
    assert_eq!(
        create(&f, &f.outsider, f.team).await.0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(create(&f, &f.admin, f.team).await.0, StatusCode::FORBIDDEN);
    assert_eq!(
        create(&f, &f.auditor, f.team).await.0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        create(&f, &f.member, Uuid::new_v4()).await.0,
        StatusCode::NOT_FOUND
    );
    // Validation (400) and conflicts (409) come before the budget.
    let (status, ..) = call_h(
        &f.s,
        &f.member,
        "POST",
        &format!("/api/v1/workspaces/{}/keys", f.team),
        json!({"name":"x","expires_in_days":0}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, ..) = call_h(
        &f.s,
        &f.member,
        "POST",
        &format!("/api/v1/workspaces/{}/keys", f.team),
        json!({"name":"  ","expires_in_days":1}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    // A member cannot create a service key, budget or not (403 before 429).
    let (_, v) = call(
        &f.s,
        &f.owner,
        "POST",
        &format!("/api/v1/workspaces/{}/service-accounts", f.team),
        json!({"name":"Robot"}),
    )
    .await;
    assert_eq!(
        create_for(&f, &f.member, f.team, id(&v)).await.0,
        StatusCode::FORBIDDEN
    );
    // Rotation: non-members and other holders are denied before the budget, as is
    // an unknown key.
    assert_eq!(
        rotate(&f, &f.outsider, f.team, keys[0]).await.0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        rotate(&f, &f.owner, f.team, keys[0]).await.0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        rotate(&f, &f.member, f.team, Uuid::new_v4()).await.0,
        StatusCode::NOT_FOUND
    );
    // A disabled key is a 409 key_disabled, not a 429.
    sqlx::query("UPDATE api_keys SET disabled_at=now() WHERE id=$1")
        .bind(keys[0])
        .execute(&pool)
        .await
        .unwrap();
    let (status, _, v) = rotate(&f, &f.member, f.team, keys[0]).await;
    assert_eq!(status, StatusCode::CONFLICT, "{v}");
    assert_eq!(reason(&v), "key_disabled");
    // A disabled workspace is 404.
    sqlx::query("UPDATE workspaces SET disabled_at=now() WHERE id=$1")
        .bind(f.team)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(create(&f, &f.member, f.team).await.0, StatusCode::NOT_FOUND);
    // Only the budget refuses an otherwise valid request.
    sqlx::query("UPDATE workspaces SET disabled_at=NULL WHERE id=$1")
        .bind(f.team)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE api_keys SET disabled_at=NULL WHERE id=$1")
        .bind(keys[0])
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        create(&f, &f.member, f.team).await.0,
        StatusCode::TOO_MANY_REQUESTS
    );
    assert_eq!(
        rotate(&f, &f.member, f.team, keys[0]).await.0,
        StatusCode::TOO_MANY_REQUESTS
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn issuance_runs_at_read_committed_and_counts_after_the_workspace_lock(pool: PgPool) {
    let f = fixture(&pool).await;
    type_limits(&pool, "team", Some(2), Some(500)).await;
    seed(&pool, f.team, f.member.user_id, 1, 60).await;
    // The transaction every issuance runs in is READ COMMITTED.
    let (mut held, _) = resources::workspace_tx(&f.s, &f.member, f.team)
        .await
        .unwrap();
    let level: String = sqlx::query_scalar("SHOW transaction_isolation")
        .fetch_one(&mut *held)
        .await
        .unwrap();
    assert_eq!(level, "read committed");
    // A create queues behind the holder of the workspace lock. The lock holder
    // issues the last allowed key and commits: the queued create must then
    // see that row (a snapshot taken before the lock would not) and be refused.
    let queued = {
        let (s, u) = (f.s.clone(), f.member.clone());
        let path = format!("/api/v1/workspaces/{}/keys", f.team);
        tokio::spawn(async move {
            call_h(
                &s,
                &u,
                "POST",
                &path,
                json!({"name":"Queued","expires_in_days":1}),
            )
            .await
        })
    };
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    assert!(
        !queued.is_finished(),
        "the create did not wait for the lock"
    );
    sqlx::query("INSERT INTO api_keys(id,workspace_id,issued_to_user_id,name,secret_hash,expires_at) VALUES(gen_random_uuid(),$1,$2,'Locked',decode(repeat('ab',32),'hex'),now()+interval '1 day')")
        .bind(f.team).bind(f.member.user_id).execute(&mut *held).await.unwrap();
    held.commit().await.unwrap();
    let (status, _, v) = queued.await.unwrap();
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{v}");
    assert_eq!(keys_of(&pool, f.team).await, 2);
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn a_limit_change_is_seen_atomically_by_a_queued_issuance(pool: PgPool) {
    let f = fixture(&pool).await;
    seed(&pool, f.team, f.member.user_id, 1, 60).await;
    // A platform type edit is in flight (it holds the installation row).
    let mut edit = resources::installation_tx(&f.s).await.unwrap();
    let queued = {
        let (s, u) = (f.s.clone(), f.member.clone());
        let path = format!("/api/v1/workspaces/{}/keys", f.team);
        tokio::spawn(async move {
            call_h(
                &s,
                &u,
                "POST",
                &path,
                json!({"name":"Queued","expires_in_days":1}),
            )
            .await
        })
    };
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    assert!(!queued.is_finished());
    sqlx::query("UPDATE workspace_type_policies SET key_issuances_per_holder=1 WHERE kind='team'")
        .execute(&mut *edit)
        .await
        .unwrap();
    edit.commit().await.unwrap();
    // The create ran after the commit: it saw the new limit, not the old one.
    let (status, _, v) = queued.await.unwrap();
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{v}");
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn concurrent_local_policy_puts_and_creates_never_exceed_the_limit(pool: PgPool) {
    let f = fixture(&pool).await;
    type_limits(&pool, "team", Some(6), Some(500)).await;
    let path = format!("/api/v1/workspaces/{}/policy", f.team);
    let mut jobs = creates(&f.member, f.team, 5);
    jobs.push((
        f.owner.clone(),
        "PUT".to_owned(),
        path.clone(),
        policy(json!({"key_issuances_per_holder":2})),
    ));
    jobs.extend(creates(&f.member, f.team, 3));
    let codes = race(&f, jobs).await;
    assert!(
        codes
            .iter()
            .all(|c| matches!(*c, StatusCode::OK | StatusCode::TOO_MANY_REQUESTS)),
        "{codes:?}"
    );
    let made = keys_of(&pool, f.team).await;
    assert!((2..=6).contains(&made), "{made}");
    // Whatever the interleaving, the final limit is the tighter one and holds.
    assert_eq!(
        create(&f, &f.member, f.team).await.0,
        StatusCode::TOO_MANY_REQUESTS
    );
    let (_, _, v) = call_h(&f.s, &f.owner, "GET", &path, Value::Null).await;
    assert_eq!(v["effective"]["key_issuances_per_holder"], 2);
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn key_issuance_view_shows_the_callers_budget_and_admins_the_workspace(pool: PgPool) {
    let f = fixture(&pool).await;
    type_limits(&pool, "team", Some(3), Some(4)).await;
    let path = format!("/api/v1/workspaces/{}/key-issuance", f.team);
    seed(&pool, f.team, f.owner.user_id, 1, 120).await;
    let (status, _, v) = call_h(&f.s, &f.member, "GET", &path, Value::Null).await;
    assert_eq!(status, StatusCode::OK, "{v}");
    assert_eq!(v["window_seconds"], DAY);
    assert_eq!(v["holder"]["used"], 0);
    assert_eq!(v["holder"]["limit"], 3);
    assert_eq!(v["holder"]["next_available_at"], Value::Null);
    // Members see only their own activity: no workspace block.
    assert!(v.get("workspace").is_none(), "{v}");
    assert!(!v.to_string().contains(&f.owner.user_id.to_string()));
    for _ in 0..3 {
        assert_eq!(create(&f, &f.member, f.team).await.0, StatusCode::OK);
    }
    let (_, _, v) = call_h(&f.s, &f.member, "GET", &path, Value::Null).await;
    assert_eq!(v["holder"]["used"], 3);
    assert!(v["holder"]["next_available_at"].is_string());
    let (_, _, refused) = create(&f, &f.member, f.team).await;
    // The view and the refusal agree on when the budget frees up.
    let retry = refused["error"]["retry_after_seconds"].as_i64().unwrap();
    let next: chrono::DateTime<chrono::Utc> =
        serde_json::from_value(v["holder"]["next_available_at"].clone()).unwrap();
    let wait = (next - chrono::Utc::now()).num_seconds();
    assert!((wait - retry).abs() <= 3, "{wait} {retry}");
    // The workspace owner sees the workspace's total as well as their own use.
    let (status, _, v) = call_h(&f.s, &f.owner, "GET", &path, Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["holder"]["used"], 1);
    assert_eq!(v["workspace"]["used"], 4);
    assert_eq!(v["workspace"]["limit"], 4);
    assert!(v["workspace"]["next_available_at"].is_string());
    // Not a member: refused, and nothing is learned about the budget.
    for u in [&f.outsider, &f.admin, &f.auditor] {
        let (status, _, v) = call_h(&f.s, u, "GET", &path, Value::Null).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{v}");
    }
    let (status, _, _) = call_h(
        &f.s,
        &f.member,
        "GET",
        &format!("/api/v1/workspaces/{}/key-issuance", Uuid::new_v4()),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // A personal workspace shows its owner both blocks; others are refused.
    let personal = format!("/api/v1/workspaces/{}/key-issuance", f.personal);
    let (status, _, v) = call_h(&f.s, &f.owner, "GET", &personal, Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["holder"]["limit"], 50);
    assert_eq!(v["workspace"]["limit"], 500);
    assert_eq!(
        call_h(&f.s, &f.admin, "GET", &personal, Value::Null)
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    // A disabled workspace is not found.
    sqlx::query("UPDATE workspaces SET disabled_at=now() WHERE id=$1")
        .bind(f.team)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        call_h(&f.s, &f.member, "GET", &path, Value::Null).await.0,
        StatusCode::NOT_FOUND
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn policy_changes_keep_their_audit_shape_and_refusals_count_in_a_metric(pool: PgPool) {
    let f = fixture(&pool).await;
    let before = crate::metrics::METRICS.key_issuance_refused_count("holder", "create");
    type_limits(&pool, "team", Some(1), Some(500)).await;
    assert_eq!(create(&f, &f.member, f.team).await.0, StatusCode::OK);
    assert_eq!(
        create(&f, &f.member, f.team).await.0,
        StatusCode::TOO_MANY_REQUESTS
    );
    assert!(crate::metrics::METRICS.key_issuance_refused_count("holder", "create") > before);
    assert_eq!(audits(&pool, "key.created").await, 1);
    let (status, _) = put(
        &f,
        &f.admin,
        "/api/v1/platform/workspace-types/team/policy",
        policy(json!({"key_issuances_per_holder":4})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let metadata: Value = sqlx::query_scalar("SELECT metadata FROM audit_events WHERE action='policy.type_updated' ORDER BY created_at DESC LIMIT 1")
        .fetch_one(&pool).await.unwrap();
    assert_eq!(
        metadata
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect::<std::collections::BTreeSet<_>>(),
        ["budget_period", "count", "kind", "previous_budget_period"].into()
    );
}
