//! Files API (`/v1/files`) and storage quota/usage on a disposable database
//! with an encrypted in-memory store: streamed upload, ownership, purpose
//! rules, store/purpose gates, expiry, quota (including concurrent uploads)
//! and hourly usage rows.
use std::{sync::Arc, time::Duration};

use axum::{
    Extension, Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use bytes::Bytes;
use futures::StreamExt;
use serde_json::Value;
use sqlx::PgPool;
use tower::ServiceExt;
use uuid::Uuid;

use super::{
    ByteStream, FileStoreError, FileStoreRuntime, Purpose,
    files::{FileError, FileStorage, NewFile, QuotaMode, public_id},
    upload::{UploadError, Uploader, receive},
    usage::record_hours,
};
use crate::{
    governance::tests::db::{Fixture, fixture},
    store::Store,
};

const BOUNDARY: &str = "gatewayBoundary7";

fn form(fields: &[(&str, &str)], file: Option<(&str, &[u8])>) -> Vec<u8> {
    let mut out = Vec::new();
    for (name, value) in fields {
        out.extend_from_slice(
            format!(
                "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"
            )
            .as_bytes(),
        );
    }
    if let Some((name, data)) = file {
        out.extend_from_slice(
            format!("--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{name}\"\r\nContent-Type: application/octet-stream\r\n\r\n").as_bytes(),
        );
        out.extend_from_slice(data);
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(format!("--{BOUNDARY}--\r\n").as_bytes());
    out
}

fn jsonl(lines: usize) -> Vec<u8> {
    let mut out = Vec::new();
    for i in 0..lines {
        out.extend_from_slice(format!("{{\"custom_id\":\"r{i}\",\"method\":\"POST\",\"url\":\"/v1/chat/completions\",\"body\":{{}}}}\n").as_bytes());
    }
    out
}

async fn bearer(f: &Fixture, workspace: Uuid) -> String {
    let key = crate::auth::NewApiKey::generate().unwrap();
    sqlx::query("INSERT INTO api_keys(id,workspace_id,issued_to_user_id,name,secret_hash) VALUES($1,$2,$3,'files',$4)").bind(key.id).bind(workspace).bind(f.owner).bind(key.digest.as_slice()).execute(&f.store.pool).await.unwrap();
    format!("Bearer {}", key.token)
}

async fn allow(pool: &PgPool, batch: bool, user_files: bool) {
    sqlx::query(
        "UPDATE installation_settings SET file_batch_enabled=$1,file_user_files_enabled=$2",
    )
    .bind(batch)
    .bind(user_files)
    .execute(pool)
    .await
    .unwrap();
}

fn app(store: &Store, rt: &FileStoreRuntime) -> Router {
    crate::http::router(store.clone()).layer(Extension(rt.clone()))
}

async fn send(
    app: &Router,
    method: &str,
    path: &str,
    auth: &str,
    body: Option<Vec<u8>>,
) -> (StatusCode, Vec<u8>) {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header("authorization", auth);
    if body.is_some() {
        request = request.header(
            "content-type",
            format!("multipart/form-data; boundary={BOUNDARY}"),
        );
    }
    let response = app
        .clone()
        .oneshot(request.body(Body::from(body.unwrap_or_default())).unwrap())
        .await
        .unwrap();
    let status = response.status();
    (
        status,
        to_bytes(response.into_body(), 64 << 20)
            .await
            .unwrap()
            .to_vec(),
    )
}

async fn json(
    app: &Router,
    method: &str,
    path: &str,
    auth: &str,
    body: Option<Vec<u8>>,
) -> (StatusCode, Value) {
    let (status, bytes) = send(app, method, path, auth, body).await;
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn code(v: &Value) -> &str {
    v["error"]["code"].as_str().unwrap_or_default()
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn upload_list_retrieve_download_and_delete(pool: PgPool) {
    let f = fixture(pool.clone()).await;
    allow(&pool, true, true).await;
    let rt = FileStoreRuntime::memory();
    let app = app(&f.store, &rt);
    let mine = bearer(&f, f.principal.workspace_id).await;
    let data = jsonl(2000);
    let (status, file) = json(
        &app,
        "POST",
        "/v1/files",
        &mine,
        Some(form(
            &[("purpose", "batch")],
            Some(("../in\u{202e}put.jsonl", &data)),
        )),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{file}");
    let id = file["id"].as_str().unwrap().to_owned();
    assert!(id.starts_with("file-") && id.len() == 37);
    assert_eq!(file["object"], "file");
    assert_eq!(file["purpose"], "batch");
    assert_eq!(file["bytes"], data.len());
    assert_eq!(file["filename"], "input.jsonl");
    assert_eq!(file["status"], "processed");
    // Batch files keep for the 7-day default retention.
    let created = file["created_at"].as_i64().unwrap();
    assert_eq!(file["expires_at"].as_i64().unwrap() - created, 7 * 86400);

    let (status, bytes) = send(&app, "GET", &format!("/v1/files/{id}/content"), &mine, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(bytes, data);
    let (status, got) = json(&app, "GET", &format!("/v1/files/{id}"), &mine, None).await;
    assert_eq!(
        (status, got["id"].as_str()),
        (StatusCode::OK, Some(id.as_str()))
    );

    // A user file with an explicit expiry.
    let (status, user) = json(
        &app,
        "POST",
        "/v1/files",
        &mine,
        Some(form(
            &[
                ("purpose", "user_data"),
                ("expires_after[anchor]", "created_at"),
                ("expires_after[seconds]", "3600"),
            ],
            Some(("notes.txt", b"hello")),
        )),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{user}");
    assert_eq!(user["purpose"], "user_data");
    assert_eq!(
        user["expires_at"].as_i64().unwrap() - user["created_at"].as_i64().unwrap(),
        3600
    );

    let (_, list) = json(&app, "GET", "/v1/files", &mine, None).await;
    assert_eq!(list["object"], "list");
    assert_eq!(list["data"].as_array().unwrap().len(), 2);
    assert_eq!(list["data"][0]["purpose"], "user_data", "newest first");
    let (_, list) = json(
        &app,
        "GET",
        "/v1/files?purpose=batch&order=asc&limit=1",
        &mine,
        None,
    )
    .await;
    assert_eq!(
        (list["data"][0]["id"].as_str(), list["has_more"].as_bool()),
        (Some(id.as_str()), Some(false))
    );
    let (_, page) = json(&app, "GET", "/v1/files?limit=1", &mine, None).await;
    assert_eq!(page["has_more"], true);
    let after = page["last_id"].as_str().unwrap();
    let (_, next) = json(
        &app,
        "GET",
        &format!("/v1/files?limit=1&after={after}"),
        &mine,
        None,
    )
    .await;
    assert_eq!(next["data"][0]["id"].as_str(), Some(id.as_str()));
    for bad in [
        "?limit=0",
        "?order=up",
        "?purpose=fine-tune",
        "?after=nope",
        "?extra=1",
    ] {
        let (status, _) = json(&app, "GET", &format!("/v1/files{bad}"), &mine, None).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}");
    }

    let (status, deleted) = json(&app, "DELETE", &format!("/v1/files/{id}"), &mine, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        deleted,
        serde_json::json!({"id": id, "object": "file", "deleted": true})
    );
    let (status, v) = json(&app, "GET", &format!("/v1/files/{id}"), &mine, None).await;
    assert_eq!((status, code(&v)), (StatusCode::NOT_FOUND, "not_found"));
    // Metadata only: the row stays, the name is cleared.
    let row: (Option<String>, bool) = sqlx::query_as(
        "SELECT filename,deleted_at IS NOT NULL FROM stored_files WHERE api_purpose='batch'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(row, (None, true));
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn files_are_scoped_to_the_key_workspace(pool: PgPool) {
    let f = fixture(pool.clone()).await;
    allow(&pool, true, true).await;
    let rt = FileStoreRuntime::memory();
    let app = app(&f.store, &rt);
    let mine = bearer(&f, f.principal.workspace_id).await;
    let theirs = bearer(&f, f.team.workspace_id).await;
    let (_, file) = json(
        &app,
        "POST",
        "/v1/files",
        &mine,
        Some(form(
            &[("purpose", "user_data")],
            Some(("a.txt", b"secret")),
        )),
    )
    .await;
    let id = file["id"].as_str().unwrap();
    for (method, path) in [
        ("GET", format!("/v1/files/{id}")),
        ("GET", format!("/v1/files/{id}/content")),
        ("DELETE", format!("/v1/files/{id}")),
    ] {
        let (status, v) = json(&app, method, &path, &theirs, None).await;
        assert_eq!(
            (status, code(&v)),
            (StatusCode::NOT_FOUND, "not_found"),
            "{method} {path}"
        );
    }
    let (_, list) = json(&app, "GET", "/v1/files", &theirs, None).await;
    assert_eq!(list["data"], serde_json::json!([]));
    // Gateway files of other purposes are invisible to the Files API.
    let files = FileStorage::new(f.store.clone(), rt.clone());
    let export = files
        .create(
            NewFile::new(Purpose::Export, Some(f.principal.workspace_id)),
            body(b"report"),
        )
        .await
        .unwrap();
    let (status, _) = json(
        &app,
        "GET",
        &format!("/v1/files/{}", public_id(export.id)),
        &mine,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // Internal files (a batch's private copy) are never Files API files; outputs are.
    let internal = files
        .create(
            NewFile::internal(Purpose::BatchOutput, Some(f.principal.workspace_id)),
            body(b"{}\n"),
        )
        .await
        .unwrap();
    assert_eq!(internal.api_purpose, None);
    let output = files
        .create(
            NewFile::new(Purpose::BatchOutput, Some(f.principal.workspace_id)),
            body(b"{}\n"),
        )
        .await
        .unwrap();
    let (status, _) = json(
        &app,
        "GET",
        &format!("/v1/files/{}/content", public_id(internal.id)),
        &mine,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (_, list) = json(&app, "GET", "/v1/files?purpose=batch_output", &mine, None).await;
    assert_eq!(list["data"].as_array().unwrap().len(), 1);
    assert_eq!(
        list["data"][0]["id"].as_str(),
        Some(public_id(output.id).as_str())
    );
    // Unauthenticated requests never reach the store.
    let (status, _) = json(&app, "GET", "/v1/files", "Bearer nope", None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn purpose_rules_and_store_gates(pool: PgPool) {
    let f = fixture(pool.clone()).await;
    let rt = FileStoreRuntime::memory();
    let app = app(&f.store, &rt);
    let mine = bearer(&f, f.principal.workspace_id).await;
    let up = |fields: Vec<(&'static str, &'static str)>,
              file: Option<(&'static str, &'static [u8])>| form(&fields, file);
    // Purpose turned off in Settings: 403, nothing stored.
    let (status, v) = json(
        &app,
        "POST",
        "/v1/files",
        &mine,
        Some(up(vec![("purpose", "batch")], Some(("a.jsonl", b"{}\n")))),
    )
    .await;
    assert_eq!(
        (status, code(&v)),
        (StatusCode::FORBIDDEN, "file_purpose_disabled"),
        "{v}"
    );
    allow(&pool, true, false).await;
    let (status, v) = json(
        &app,
        "POST",
        "/v1/files",
        &mine,
        Some(up(
            vec![("purpose", "vision")],
            Some(("a.png", b"\x89PNG\r\n\x1a\n")),
        )),
    )
    .await;
    assert_eq!(
        (status, code(&v)),
        (StatusCode::FORBIDDEN, "file_purpose_disabled")
    );
    allow(&pool, true, true).await;
    for (fields, file, status, expected) in [
        (
            vec![("purpose", "fine-tune")],
            Some(("a.jsonl", &b"{}"[..])),
            StatusCode::BAD_REQUEST,
            "unsupported_capability",
        ),
        (
            vec![("purpose", "batch_output")],
            Some(("a.jsonl", &b"{}"[..])),
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
        ),
        (
            vec![("purpose", "batch")],
            Some(("a.jsonl", &b"not json"[..])),
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
        ),
        (
            vec![("purpose", "vision")],
            Some(("a.svg", &b"<svg/>"[..])),
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
        ),
        (
            vec![("purpose", "user_data")],
            Some(("empty.txt", &b""[..])),
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
        ),
        (
            vec![],
            Some(("a.jsonl", &b"{}"[..])),
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
        ),
        (
            vec![("purpose", "batch")],
            None,
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
        ),
        (
            vec![
                ("purpose", "batch"),
                ("expires_after[anchor]", "created_at"),
                ("expires_after[seconds]", "60"),
            ],
            Some(("a.jsonl", &b"{}"[..])),
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
        ),
        (
            vec![("purpose", "batch"), ("expires_after[seconds]", "3600")],
            Some(("a.jsonl", &b"{}"[..])),
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
        ),
        (
            vec![("purpose", "batch"), ("other", "x")],
            Some(("a.jsonl", &b"{}"[..])),
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
        ),
    ] {
        let (got, v) = json(
            &app,
            "POST",
            "/v1/files",
            &mine,
            Some(up(fields.clone(), file)),
        )
        .await;
        assert_eq!((got, code(&v)), (status, expected), "{fields:?} {v}");
    }
    // Vision accepts real image signatures.
    let (status, v) = json(
        &app,
        "POST",
        "/v1/files",
        &mine,
        Some(up(
            vec![("purpose", "vision")],
            Some(("a.png", b"\x89PNG\r\n\x1a\n0000")),
        )),
    )
    .await;
    assert_eq!(
        (status, v["purpose"].as_str()),
        (StatusCode::OK, Some("vision"))
    );
    // No failed upload left a live row or a reservation behind.
    let live: i64 =
        sqlx::query_scalar("SELECT count(*) FROM stored_files WHERE deleted_at IS NULL")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(live, 1);

    // Store off: 503 on upload and content; metadata reads still work.
    let off = self::app(&f.store, &FileStoreRuntime::off());
    let (status, v) = json(
        &off,
        "POST",
        "/v1/files",
        &mine,
        Some(up(vec![("purpose", "batch")], Some(("a.jsonl", b"{}")))),
    )
    .await;
    assert_eq!(
        (status, code(&v)),
        (
            StatusCode::SERVICE_UNAVAILABLE,
            "file_storage_not_configured"
        )
    );
    let id = v["id"].as_str().unwrap_or_default().to_owned();
    assert!(id.is_empty());
    let (_, list) = json(&off, "GET", "/v1/files", &mine, None).await;
    let vision = list["data"][0]["id"].as_str().unwrap().to_owned();
    let (status, v) = json(
        &off,
        "GET",
        &format!("/v1/files/{vision}/content"),
        &mine,
        None,
    )
    .await;
    assert_eq!(
        (status, code(&v)),
        (
            StatusCode::SERVICE_UNAVAILABLE,
            "file_storage_not_configured"
        )
    );
}

fn body(data: &[u8]) -> ByteStream {
    futures::stream::iter([Ok(Bytes::copy_from_slice(data))]).boxed()
}

async fn set_quota(pool: &PgPool, kind: &str, bytes: Option<i64>) {
    sqlx::query("UPDATE workspace_type_policies SET storage_bytes=$2 WHERE kind=$1")
        .bind(kind)
        .bind(bytes)
        .execute(pool)
        .await
        .unwrap();
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn quota_aborts_uploads_and_counts_live_files(pool: PgPool) {
    let f = fixture(pool.clone()).await;
    allow(&pool, true, true).await;
    let rt = FileStoreRuntime::memory();
    let files = FileStorage::new(f.store.clone(), rt.clone());
    let ws = f.principal.workspace_id;
    // Type defaults start at 1 GiB.
    assert_eq!(
        files.workspace_storage(ws).await.unwrap().quota_bytes,
        Some(1 << 30)
    );
    set_quota(&pool, "personal", Some(100_000)).await;
    let app = app(&f.store, &rt);
    let mine = bearer(&f, ws).await;
    let (status, _) = json(
        &app,
        "POST",
        "/v1/files",
        &mine,
        Some(form(
            &[("purpose", "user_data")],
            Some(("a.bin", &[1u8; 60_000])),
        )),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, v) = json(
        &app,
        "POST",
        "/v1/files",
        &mine,
        Some(form(
            &[("purpose", "user_data")],
            Some(("b.bin", &[2u8; 60_000])),
        )),
    )
    .await;
    assert_eq!(
        (status, code(&v)),
        (StatusCode::PAYLOAD_TOO_LARGE, "storage_quota_exceeded"),
        "{v}"
    );
    assert_eq!(v["error"]["type"], "insufficient_quota");
    let usage = files.workspace_storage(ws).await.unwrap();
    assert_eq!(
        usage.used_bytes, 60_000,
        "the refused upload released its reservation"
    );
    let refused: (i64, i64) = sqlx::query_as("SELECT count(*),max(reserved_bytes) FROM stored_files WHERE deleted_at IS NOT NULL AND committed_at IS NULL").fetch_one(&pool).await.unwrap();
    assert_eq!(refused.0, 1);
    assert!(refused.1 <= 40_000, "never reserved past the quota");
    // Reservations never shrink and never change after commit.
    let err = sqlx::query("UPDATE stored_files SET reserved_bytes=0 WHERE deleted_at IS NULL")
        .execute(&pool)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("only grow while pending"), "{err}");
    // A platform override replaces the type default; a local cap tightens it.
    sqlx::query("INSERT INTO workspace_platform_policy_overrides(workspace_id,storage_bytes) VALUES($1,200000)").bind(ws).execute(&pool).await.unwrap();
    assert_eq!(
        files.workspace_storage(ws).await.unwrap().quota_bytes,
        Some(200_000)
    );
    sqlx::query(
        "INSERT INTO workspace_local_policies(workspace_id,storage_bytes) VALUES($1,150000)",
    )
    .bind(ws)
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(
        files.workspace_storage(ws).await.unwrap().quota_bytes,
        Some(150_000)
    );
    let (status, _) = json(
        &app,
        "POST",
        "/v1/files",
        &mine,
        Some(form(
            &[("purpose", "user_data")],
            Some(("c.bin", &[3u8; 60_000])),
        )),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    // Count-only outputs never fail on the quota but are counted.
    let out = files
        .create(
            NewFile {
                quota: QuotaMode::CountOnly,
                ..NewFile::new(Purpose::BatchOutput, Some(ws))
            },
            body(&[4u8; 50_000]),
        )
        .await
        .unwrap();
    assert_eq!(out.api_purpose.as_deref(), Some("batch_output"));
    assert_eq!(
        files.workspace_storage(ws).await.unwrap().used_bytes,
        170_000
    );
    // Deleting frees the space at once; expired files stop counting.
    files.delete(out.id, Some(ws)).await.unwrap();
    assert_eq!(
        files.workspace_storage(ws).await.unwrap().used_bytes,
        120_000
    );
    sqlx::query("UPDATE stored_files SET expires_at=now()-interval '1 second' WHERE deleted_at IS NULL AND workspace_id=$1").bind(ws).execute(&pool).await.unwrap();
    assert_eq!(files.workspace_storage(ws).await.unwrap().used_bytes, 0);
    let (_, list) = json(&app, "GET", "/v1/files", &mine, None).await;
    assert_eq!(
        list["data"],
        serde_json::json!([]),
        "expired files are unreadable at once"
    );
    // No cap at all: NULL on every layer.
    sqlx::query("UPDATE workspace_platform_policy_overrides SET storage_bytes=NULL")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE workspace_local_policies SET storage_bytes=NULL")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(files.workspace_storage(ws).await.unwrap().quota_bytes, None);
}

/// A body that yields `chunks` of `size` bytes, pausing between them so two
/// uploads interleave.
fn slow(chunks: usize, size: usize) -> ByteStream {
    futures::stream::iter(0..chunks)
        .then(move |i| async move {
            tokio::time::sleep(Duration::from_millis(5)).await;
            Ok::<_, FileStoreError>(Bytes::from(vec![i as u8; size]))
        })
        .boxed()
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn concurrent_uploads_never_jointly_exceed_the_quota(pool: PgPool) {
    let f = fixture(pool.clone()).await;
    allow(&pool, true, true).await;
    const MIB: usize = 1 << 20;
    set_quota(&pool, "personal", Some(10 * MIB as i64)).await;
    let files = Arc::new(FileStorage::new(
        f.store.clone(),
        FileStoreRuntime::memory(),
    ));
    let ws = f.principal.workspace_id;
    for _ in 0..3 {
        let tasks: Vec<_> = (0..2)
            .map(|_| {
                let files = files.clone();
                tokio::spawn(async move {
                    files
                        .create(NewFile::new(Purpose::UserFile, Some(ws)), slow(6, MIB))
                        .await
                })
            })
            .collect();
        let mut results = Vec::new();
        for t in tasks {
            results.push(t.await.unwrap());
        }
        let ok: Vec<_> = results.iter().filter_map(|r| r.as_ref().ok()).collect();
        assert_eq!(ok.len(), 1, "{results:?}");
        assert!(
            results
                .iter()
                .any(|r| matches!(r, Err(FileError::QuotaExceeded)))
        );
        let usage = files.workspace_storage(ws).await.unwrap();
        assert_eq!(usage.used_bytes, 6 * MIB as u64);
        files.delete(ok[0].id, Some(ws)).await.unwrap();
    }
    let live: i64 =
        sqlx::query_scalar("SELECT count(*) FROM stored_files WHERE deleted_at IS NULL")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(live, 0);
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn upload_pipeline_limits_and_interruptions(pool: PgPool) {
    let f = fixture(pool.clone()).await;
    allow(&pool, true, true).await;
    let files = FileStorage::new(f.store.clone(), FileStoreRuntime::memory());
    let who = Uploader {
        workspace_id: f.principal.workspace_id,
        api_key_id: Some(f.principal.key_id),
        user_id: f.principal.user_id,
    };
    let ct = format!("multipart/form-data; boundary={BOUNDARY}");
    let stream =
        |data: Vec<u8>| futures::stream::iter([Ok::<_, std::io::Error>(Bytes::from(data))]);
    let big = form(&[("purpose", "user_data")], Some(("a.bin", &[0u8; 5000])));
    assert_eq!(
        receive(&files, who, Some(&ct), stream(big), 4096)
            .await
            .unwrap_err(),
        UploadError::TooLarge
    );
    // A client that disconnects mid-file leaves nothing behind.
    let mut cut = form(&[("purpose", "user_data")], Some(("a.bin", &[0u8; 5000])));
    cut.truncate(cut.len() - 40);
    let chunks = futures::stream::iter([Ok(Bytes::from(cut)), Err(std::io::Error::other("reset"))]);
    let err = receive(&files, who, Some(&ct), chunks, 1 << 20)
        .await
        .unwrap_err();
    assert!(matches!(err, UploadError::Invalid { .. }), "{err:?}");
    assert!(matches!(
        receive(
            &files,
            who,
            Some("application/json"),
            stream(vec![]),
            1 << 20
        )
        .await
        .unwrap_err(),
        UploadError::Invalid { .. }
    ));
    let live: i64 =
        sqlx::query_scalar("SELECT count(*) FROM stored_files WHERE deleted_at IS NULL")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(live, 0);
    let ok = form(&[("purpose", "evals")], Some(("e.jsonl", b"{}")));
    let stored = receive(&files, who, Some(&ct), stream(ok), 1 << 20)
        .await
        .unwrap();
    assert_eq!(
        (stored.api_purpose.as_deref(), stored.created_by_api_key_id),
        (Some("evals"), Some(f.principal.key_id))
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn hourly_usage_is_append_only_byte_seconds(pool: PgPool) {
    let f = fixture(pool.clone()).await;
    allow(&pool, true, true).await;
    let files = FileStorage::new(f.store.clone(), FileStoreRuntime::memory());
    let ws = f.principal.workspace_id;
    let a = files
        .create(
            NewFile::new(Purpose::UserFile, Some(ws)),
            body(&[1u8; 1000]),
        )
        .await
        .unwrap();
    let b = files
        .create(
            NewFile::new(Purpose::BatchInput, Some(ws)),
            body(&[2u8; 10]),
        )
        .await
        .unwrap();
    let base: chrono::DateTime<chrono::Utc> =
        sqlx::query_scalar("SELECT date_trunc('hour', now(), 'UTC') - interval '5 hours'")
            .fetch_one(&pool)
            .await
            .unwrap();
    // a: stored for the last 5 hours and still live; b: 90 minutes from base+30m.
    sqlx::query("ALTER TABLE storage_usage_progress DISABLE TRIGGER storage_usage_progress_guard")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE storage_usage_progress SET recorded_through=$1")
        .bind(base - chrono::TimeDelta::hours(1))
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("ALTER TABLE storage_usage_progress ENABLE TRIGGER storage_usage_progress_guard")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("ALTER TABLE stored_files DISABLE TRIGGER stored_files_guard")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE stored_files SET committed_at=$2,created_at=$2 WHERE id=$1")
        .bind(a.id)
        .bind(base)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE stored_files SET committed_at=$2,created_at=$2,deleted_at=$3 WHERE id=$1")
        .bind(b.id)
        .bind(base + chrono::TimeDelta::minutes(30))
        .bind(base + chrono::TimeDelta::minutes(120))
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("ALTER TABLE stored_files ENABLE TRIGGER stored_files_guard")
        .execute(&pool)
        .await
        .unwrap();
    let hours = record_hours(&f.store, 48).await.unwrap();
    assert!(hours >= 4, "{hours}");
    let rows: Vec<(String, chrono::DateTime<chrono::Utc>, String, i32)> = sqlx::query_as("SELECT purpose,hour_start,byte_seconds::text,file_count FROM storage_usage_hours WHERE workspace_id=$1 ORDER BY purpose,hour_start").bind(ws).fetch_all(&pool).await.unwrap();
    let batch: Vec<_> = rows
        .iter()
        .filter(|r| r.0 == "batch_input")
        .map(|r| (r.1 - base, r.2.clone()))
        .collect();
    assert_eq!(
        batch,
        vec![
            (chrono::TimeDelta::zero(), "18000".to_owned()),
            (chrono::TimeDelta::hours(1), "36000".to_owned())
        ]
    );
    let user: Vec<_> = rows
        .iter()
        .filter(|r| r.0 == "user_file")
        .map(|r| r.2.clone())
        .collect();
    assert!(
        user.len() >= 4 && user.iter().all(|bs| bs == "3600000"),
        "{user:?}"
    );
    // Running again records nothing twice.
    assert_eq!(record_hours(&f.store, 48).await.unwrap(), 0);
    let again: i64 = sqlx::query_scalar("SELECT count(*) FROM storage_usage_hours")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(again as usize, rows.len());
    // History is append-only and progress only moves forward.
    let err = sqlx::query("UPDATE storage_usage_hours SET byte_seconds=1")
        .execute(&pool)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("append-only"));
    let err = sqlx::query("DELETE FROM storage_usage_hours")
        .execute(&pool)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("append-only"));
    let err = sqlx::query(
        "UPDATE storage_usage_progress SET recorded_through=recorded_through-interval '1 hour'",
    )
    .execute(&pool)
    .await
    .unwrap_err();
    assert!(err.to_string().contains("only moves forward"));
}

/// The file part first, then the fields (as OpenAI's Node SDK sends them).
fn file_first(name: &str, data: &[u8], fields: &[(&str, &str)]) -> Vec<u8> {
    let mut out = format!("--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{name}\"\r\nContent-Type: text/plain\r\n\r\n").into_bytes();
    out.extend_from_slice(data);
    out.extend_from_slice(b"\r\n");
    for (k, v) in fields {
        out.extend_from_slice(
            format!("--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"{k}\"\r\n\r\n{v}\r\n")
                .as_bytes(),
        );
    }
    out.extend_from_slice(format!("--{BOUNDARY}--\r\n").as_bytes());
    out
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn file_first_forms_are_staged_then_resolved(pool: PgPool) {
    let f = fixture(pool.clone()).await;
    allow(&pool, true, true).await;
    let rt = FileStoreRuntime::memory();
    let app = app(&f.store, &rt);
    let mine = bearer(&f, f.principal.workspace_id).await;
    let lines = jsonl(3);
    // Provisional batch purpose kept (.jsonl + batch): committed in place.
    let (status, v) = json(
        &app,
        "POST",
        "/v1/files",
        &mine,
        Some(file_first("in.jsonl", &lines, &[("purpose", "batch")])),
    )
    .await;
    assert_eq!(
        (status, v["purpose"].as_str()),
        (StatusCode::OK, Some("batch")),
        "{v}"
    );
    let (_, bytes) = send(
        &app,
        "GET",
        &format!("/v1/files/{}/content", v["id"].as_str().unwrap()),
        &mine,
        None,
    )
    .await;
    assert_eq!(bytes, lines);
    // A .jsonl eval file moves to user files (streamed copy) with its expiry.
    let (status, v) = json(
        &app,
        "POST",
        "/v1/files",
        &mine,
        Some(file_first(
            "evals.jsonl",
            b"{\"q\":1}\n",
            &[
                ("purpose", "evals"),
                ("expires_after[anchor]", "created_at"),
                ("expires_after[seconds]", "7200"),
            ],
        )),
    )
    .await;
    assert_eq!(
        (status, v["purpose"].as_str()),
        (StatusCode::OK, Some("evals")),
        "{v}"
    );
    assert_eq!(
        v["expires_at"].as_i64().unwrap() - v["created_at"].as_i64().unwrap(),
        7200
    );
    let (_, bytes) = send(
        &app,
        "GET",
        &format!("/v1/files/{}/content", v["id"].as_str().unwrap()),
        &mine,
        None,
    )
    .await;
    assert_eq!(bytes, b"{\"q\":1}\n");
    let purposes: Vec<(String, Option<String>, bool)> = sqlx::query_as(
        "SELECT purpose,api_purpose,deleted_at IS NULL FROM stored_files ORDER BY created_at",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        purposes,
        vec![
            ("batch_input".into(), Some("batch".into()), true),
            ("batch_input".into(), None, false),
            ("user_file".into(), Some("evals".into()), true),
        ]
    );
    // A text file declared as batch is sniffed after the fields and refused.
    let (status, v) = json(
        &app,
        "POST",
        "/v1/files",
        &mine,
        Some(file_first("notes.txt", b"hello", &[("purpose", "batch")])),
    )
    .await;
    assert_eq!(
        (status, code(&v)),
        (StatusCode::BAD_REQUEST, "invalid_request_error")
    );
    // Disabled purpose, missing purpose, a second file: nothing is kept.
    allow(&pool, false, true).await;
    for body in [
        file_first("in.jsonl", &lines, &[("purpose", "batch")]),
        file_first("in.txt", b"x", &[]),
        file_first(
            "in.txt",
            b"x",
            &[("purpose", "user_data"), ("purpose", "user_data")],
        ),
    ] {
        let (status, v) = json(&app, "POST", "/v1/files", &mine, Some(body)).await;
        assert!(
            matches!(status, StatusCode::FORBIDDEN | StatusCode::BAD_REQUEST),
            "{status} {v}"
        );
    }
    let live: i64 =
        sqlx::query_scalar("SELECT count(*) FROM stored_files WHERE deleted_at IS NULL")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(live, 2);
    let usage = FileStorage::new(f.store.clone(), rt)
        .workspace_storage(f.principal.workspace_id)
        .await
        .unwrap();
    assert_eq!(usage.used_bytes, lines.len() as u64 + 8);
}
