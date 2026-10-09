//! Workspace › Files, the Storage limit and Usage › Storage: visibility
//! (admins see all, members their own, platform roles nothing), dashboard
//! upload/download/delete, stacked tighten-only quota and not-charged usage.
use super::*;
use crate::filestore::{
    ByteStream, FileStoreRuntime, Purpose,
    files::{FileStorage, NewFile, public_id},
};
use futures::StreamExt;

async fn call_files(
    s: &Store,
    u: &BrowserPrincipal,
    rt: &FileStoreRuntime,
    method: &str,
    path: &str,
    body: Option<(String, Vec<u8>)>,
) -> (StatusCode, Vec<u8>) {
    let router = routes()
        .merge(files::upload_routes())
        .layer(Extension(u.clone()))
        .layer(Extension(rt.clone()))
        .with_state(s.clone());
    let mut request = Request::builder().method(method).uri(path);
    let body = match body {
        Some((ct, bytes)) => {
            request = request.header("content-type", ct);
            Body::from(bytes)
        }
        None => Body::empty(),
    };
    let response = router.oneshot(request.body(body).unwrap()).await.unwrap();
    let status = response.status();
    (
        status,
        to_bytes(response.into_body(), 16 << 20)
            .await
            .unwrap()
            .to_vec(),
    )
}

async fn files_json(
    s: &Store,
    u: &BrowserPrincipal,
    rt: &FileStoreRuntime,
    method: &str,
    path: &str,
) -> (StatusCode, Value) {
    let (status, bytes) = call_files(s, u, rt, method, path, None).await;
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn data(bytes: &[u8]) -> ByteStream {
    futures::stream::iter([Ok(bytes::Bytes::copy_from_slice(bytes))]).boxed()
}

async fn enable(pool: &PgPool) {
    sqlx::query(
        "UPDATE installation_settings SET file_batch_enabled=true,file_user_files_enabled=true",
    )
    .execute(pool)
    .await
    .unwrap();
}

async fn stored(files: &FileStorage, ws: Uuid, user: Uuid, name: &str) -> String {
    let file = files
        .create(
            NewFile {
                created_by_user_id: Some(user),
                filename: Some(name.into()),
                ..NewFile::new(Purpose::UserFile, Some(ws))
            },
            data(b"contents"),
        )
        .await
        .unwrap();
    public_id(file.id)
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn files_follow_workspace_visibility(pool: PgPool) {
    let f = fixture(&pool).await;
    enable(&pool).await;
    let rt = FileStoreRuntime::memory();
    let files = FileStorage::new(f.s.clone(), rt.clone());
    let owners = stored(&files, f.team, f.owner.user_id, "owner.txt").await;
    let members = stored(&files, f.team, f.member.user_id, "member.txt").await;
    let list = format!("/api/v1/workspaces/{}/files", f.team);
    let (status, v) = files_json(&f.s, &f.owner, &rt, "GET", &list).await;
    assert_eq!(status, StatusCode::OK, "{v}");
    assert_eq!(v["scope"], "workspace");
    assert_eq!(v["data"].as_array().unwrap().len(), 2);
    assert_eq!(v["storage"]["quota_bytes"], 1u64 << 30);
    assert_eq!(v["storage"]["used_bytes"], 16);
    assert_eq!(
        v["store"],
        json!({"configured": true, "batch": true, "user_files": true})
    );
    let (_, v) = files_json(&f.s, &f.member, &rt, "GET", &list).await;
    assert_eq!(v["scope"], "own");
    assert_eq!(v["data"].as_array().unwrap().len(), 1);
    assert_eq!(v["data"][0]["id"].as_str(), Some(members.as_str()));
    assert_eq!(v["data"][0]["mine"], true);
    assert!(
        v["storage"]["used_bytes"].is_null(),
        "members do not see workspace totals"
    );
    let (_, v) = files_json(&f.s, &f.owner, &rt, "GET", &format!("{list}?search=MEMB")).await;
    assert_eq!(v["data"].as_array().unwrap().len(), 1);
    let (_, v) = files_json(&f.s, &f.owner, &rt, "GET", &format!("{list}?purpose=batch")).await;
    assert_eq!(v["data"], json!([]));
    assert_eq!(
        files_json(&f.s, &f.owner, &rt, "GET", &format!("{list}?purpose=nope"))
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    // Platform roles without membership and outsiders see nothing.
    for u in [&f.admin, &f.auditor, &f.outsider] {
        let (status, _) = files_json(&f.s, u, &rt, "GET", &list).await;
        assert!(
            matches!(status, StatusCode::FORBIDDEN | StatusCode::NOT_FOUND),
            "{status}"
        );
        let (status, _) =
            files_json(&f.s, u, &rt, "GET", &format!("{list}/{owners}/content")).await;
        assert!(
            matches!(status, StatusCode::FORBIDDEN | StatusCode::NOT_FOUND),
            "{status}"
        );
    }
    // Members cannot read or delete files they cannot see.
    let (status, _) = files_json(
        &f.s,
        &f.member,
        &rt,
        "GET",
        &format!("{list}/{owners}/content"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = files_json(&f.s, &f.member, &rt, "DELETE", &format!("{list}/{owners}")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // Downloads are opaque attachments.
    let (status, bytes) = call_files(
        &f.s,
        &f.member,
        &rt,
        "GET",
        &format!("{list}/{members}/content"),
        None,
    )
    .await;
    assert_eq!(
        (status, bytes.as_slice()),
        (StatusCode::OK, &b"contents"[..])
    );
    // The member deletes their own file; the admin deletes any file; both audited.
    assert_eq!(
        files_json(&f.s, &f.member, &rt, "DELETE", &format!("{list}/{members}"))
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        files_json(&f.s, &f.owner, &rt, "DELETE", &format!("{list}/{owners}"))
            .await
            .0,
        StatusCode::OK
    );
    let audited: Vec<(String, Value)> = sqlx::query_as(
        "SELECT action,metadata FROM audit_events WHERE resource_type='file' ORDER BY created_at",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(audited.len(), 2);
    assert!(
        audited
            .iter()
            .all(|(a, m)| a == "file.deleted" && m == &json!({"kind": "user_data"}))
    );
    // Personal workspaces stay owner-private.
    let personal = stored(&files, f.personal, f.owner.user_id, "mine.txt").await;
    let path = format!("/api/v1/workspaces/{}/files/{personal}/content", f.personal);
    assert_eq!(
        call_files(&f.s, &f.owner, &rt, "GET", &path, None).await.0,
        StatusCode::OK
    );
    assert_ne!(
        call_files(&f.s, &f.admin, &rt, "GET", &path, None).await.0,
        StatusCode::OK
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn dashboard_upload_uses_the_files_pipeline(pool: PgPool) {
    let f = fixture(&pool).await;
    let rt = FileStoreRuntime::memory();
    let path = format!("/api/v1/workspaces/{}/files/upload", f.team);
    let form = |purpose: &str, data: &[u8]| {
        let mut out = format!("--b\r\nContent-Disposition: form-data; name=\"purpose\"\r\n\r\n{purpose}\r\n--b\r\nContent-Disposition: form-data; name=\"file\"; filename=\"in.jsonl\"\r\n\r\n").into_bytes();
        out.extend_from_slice(data);
        out.extend_from_slice(b"\r\n--b--\r\n");
        ("multipart/form-data; boundary=b".to_owned(), out)
    };
    // Purpose turned off: a structured 403 with the Files API reason.
    let (status, bytes) = call_files(
        &f.s,
        &f.member,
        &rt,
        "POST",
        &path,
        Some(form("batch", b"{}\n")),
    )
    .await;
    let v: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        (status, v["error"]["reason"].as_str()),
        (StatusCode::FORBIDDEN, Some("file_purpose_disabled"))
    );
    enable(&pool).await;
    let (status, bytes) = call_files(
        &f.s,
        &f.member,
        &rt,
        "POST",
        &path,
        Some(form("batch", b"{\"custom_id\":\"a\"}\n")),
    )
    .await;
    let v: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(status, StatusCode::OK, "{v}");
    assert_eq!(
        (
            v["purpose"].as_str(),
            v["mine"].as_bool(),
            v["via_key"].as_bool()
        ),
        (Some("batch"), Some(true), Some(false))
    );
    let audited: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_events WHERE action='file.uploaded' AND metadata=$1",
    )
    .bind(json!({"kind":"batch"}))
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(audited, 1);
    // Store off: 503 with its reason. Outsiders cannot upload.
    let (status, bytes) = call_files(
        &f.s,
        &f.member,
        &FileStoreRuntime::off(),
        "POST",
        &path,
        Some(form("user_data", b"x")),
    )
    .await;
    let v: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        (status, v["error"]["reason"].as_str()),
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Some("file_storage_not_configured")
        )
    );
    let (status, _) = call_files(
        &f.s,
        &f.outsider,
        &rt,
        "POST",
        &path,
        Some(form("user_data", b"x")),
    )
    .await;
    assert!(matches!(
        status,
        StatusCode::FORBIDDEN | StatusCode::NOT_FOUND
    ));
    // Quota refusal keeps the Files API code.
    sqlx::query("INSERT INTO workspace_local_policies(workspace_id,storage_bytes) VALUES($1,20)")
        .bind(f.team)
        .execute(&pool)
        .await
        .unwrap();
    let (status, bytes) = call_files(
        &f.s,
        &f.member,
        &rt,
        "POST",
        &path,
        Some(form("user_data", &[7u8; 30])),
    )
    .await;
    let v: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        (status, v["error"]["reason"].as_str()),
        (
            StatusCode::PAYLOAD_TOO_LARGE,
            Some("storage_quota_exceeded")
        )
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn storage_limit_stacks_and_is_tighten_only(pool: PgPool) {
    let f = fixture(&pool).await;
    let gib = 1i64 << 30;
    let (status, t) = call(
        &f.s,
        &f.admin,
        "GET",
        "/api/v1/platform/workspace-types/team/policy",
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{t}");
    assert_eq!(t["policy"]["storage_bytes"], gib);
    let ws_policy = format!("/api/v1/workspaces/{}/policy", f.team);
    let (_, w) = call(&f.s, &f.owner, "GET", &ws_policy, json!({})).await;
    assert_eq!(w["effective"]["storage_bytes"], gib);
    assert_eq!(
        w["storage"],
        json!({"quota_bytes": gib, "used_bytes": 0, "usage_visible": true})
    );
    let (_, w) = call(&f.s, &f.member, "GET", &ws_policy, json!({})).await;
    assert!(w["storage"]["used_bytes"].is_null());
    let body = |storage: Value| json!({"requests_per_minute":null,"tokens_per_minute":null,"concurrent_requests":null,"budgets":[],"storage_bytes":storage});
    let (status, v) = call(&f.s, &f.owner, "PUT", &ws_policy, body(json!(gib * 2))).await;
    assert_eq!(
        (
            status,
            v["error"]["reason"].as_str(),
            v["error"]["limit"].as_str()
        ),
        (
            StatusCode::BAD_REQUEST,
            Some("exceeds_parent_rate"),
            Some("storage_bytes")
        )
    );
    let (status, v) = call(&f.s, &f.owner, "PUT", &ws_policy, body(json!(1000))).await;
    assert_eq!(status, StatusCode::OK, "{v}");
    let (_, w) = call(&f.s, &f.owner, "GET", &ws_policy, json!({})).await;
    assert_eq!(
        (
            w["policy"]["storage_bytes"].clone(),
            w["storage"]["quota_bytes"].clone()
        ),
        (json!(1000), json!(1000))
    );
    let (status, v) = call(&f.s, &f.owner, "PUT", &ws_policy, body(Value::Null)).await;
    assert_eq!(
        (status, v["error"]["reason"].as_str()),
        (
            StatusCode::FORBIDDEN,
            Some("stored_rate_loosen_not_allowed")
        )
    );
    let (status, v) = call(&f.s, &f.owner, "PUT", &ws_policy, body(json!(0))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{v}");
    // A platform override replaces the type default (still capped by local).
    let platform = format!("/api/v1/platform/workspaces/{}/policy", f.team);
    let (status, v) = call(&f.s, &f.admin, "PUT", &platform, body(json!(500))).await;
    assert_eq!(status, StatusCode::OK, "{v}");
    let (_, p) = call(&f.s, &f.admin, "GET", &platform, json!({})).await;
    assert_eq!(
        (
            p["effective"]["storage_bytes"].clone(),
            p["provenance"]["type_default"]["storage_bytes"].clone()
        ),
        (json!(500), json!(gib))
    );
    // Type defaults are platform-edited; clearing removes the cap.
    let (status, v) = call(
        &f.s,
        &f.admin,
        "PUT",
        "/api/v1/platform/workspace-types/project/policy",
        body(Value::Null),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{v}");
    let (_, t) = call(
        &f.s,
        &f.admin,
        "GET",
        "/api/v1/platform/workspace-types/project/policy",
        json!({}),
    )
    .await;
    assert!(t["policy"]["storage_bytes"].is_null());
    // Installation and key layers have no storage limit.
    let (status, _) = call(
        &f.s,
        &f.admin,
        "PUT",
        "/api/v1/platform/installation/policy",
        body(json!(10)),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let audited: i64 = sqlx::query_scalar("SELECT count(*) FROM audit_events WHERE action IN ('policy.local_updated','policy.override_updated','policy.type_updated')").fetch_one(&pool).await.unwrap();
    assert_eq!(audited, 3);
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn storage_usage_is_not_charged_and_private(pool: PgPool) {
    let f = fixture(&pool).await;
    let hour: chrono::DateTime<chrono::Utc> =
        sqlx::query_scalar("SELECT date_trunc('day', now(), 'UTC') + interval '1 hour'")
            .fetch_one(&pool)
            .await
            .unwrap();
    let gb_day = crate::filestore::usage::GB_DAY_BYTE_SECONDS;
    for (ws, purpose, bs) in [
        (f.team, "batch_input", gb_day),
        (f.team, "user_file", gb_day / 2),
        (f.personal, "user_file", gb_day / 4),
    ] {
        sqlx::query("INSERT INTO storage_usage_hours(workspace_id,purpose,hour_start,byte_seconds,file_count) VALUES($1,$2,$3,$4,1)").bind(ws).bind(purpose).bind(hour).bind(bs).execute(&pool).await.unwrap();
    }
    let today = chrono::Utc::now().date_naive();
    let range = format!(
        "start_date={}&end_date={}",
        today,
        today + chrono::TimeDelta::days(1)
    );
    let ws_path = format!("/api/v1/workspaces/{}/usage/storage?{range}", f.team);
    let (status, v) = call(&f.s, &f.owner, "GET", &ws_path, json!({})).await;
    assert_eq!(status, StatusCode::OK, "{v}");
    assert_eq!(v["cost_state"], "not_charged");
    assert_eq!(v["total"]["gb_days"], "1.5");
    assert_eq!(
        v["total"]["byte_seconds"],
        (gb_day + gb_day / 2).to_string()
    );
    assert_eq!(
        v["by_purpose"][0],
        json!({"purpose":"batch_input","byte_seconds":gb_day.to_string(),"gb_days":"1"})
    );
    assert_eq!(v["daily"][0]["date"], today.to_string());
    assert_eq!(v["current"]["quota_bytes"], 1u64 << 30);
    // Members without workspace-wide visibility and outsiders are refused.
    for u in [&f.member, &f.outsider, &f.admin] {
        assert_ne!(
            call(&f.s, u, "GET", &ws_path, json!({})).await.0,
            StatusCode::OK
        );
    }
    let platform = format!("/api/v1/platform/usage/storage?{range}");
    let (status, v) = call(&f.s, &f.auditor, "GET", &platform, json!({})).await;
    assert_eq!(status, StatusCode::OK, "{v}");
    assert_eq!(
        (v["cost_state"].as_str(), v["total"]["gb_days"].as_str()),
        (Some("not_charged"), Some("1.75"))
    );
    let rows = v["workspaces"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    let team = rows.iter().find(|r| r["kind"] == "team").unwrap();
    assert_eq!(team["by_purpose"].as_array().unwrap().len(), 2);
    let personal = rows.iter().find(|r| r["kind"] == "personal").unwrap();
    assert_eq!(
        (
            personal["gb_days"].as_str(),
            personal["by_purpose"].is_null()
        ),
        (Some("0.25"), true),
        "personal: totals only"
    );
    assert_eq!(
        call(&f.s, &f.owner, "GET", &platform, json!({})).await.0,
        StatusCode::FORBIDDEN
    );
    for bad in [
        "start_date=2026-01-01&end_date=2026-01-01",
        "start_date=x&end_date=2026-01-02",
    ] {
        assert_eq!(
            call(
                &f.s,
                &f.admin,
                "GET",
                &format!("/api/v1/platform/usage/storage?{bad}"),
                json!({})
            )
            .await
            .0,
            StatusCode::BAD_REQUEST
        );
    }
}
