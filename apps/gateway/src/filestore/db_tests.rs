//! Metadata, per-purpose policy, retention sweeps and verification against a
//! real database (encrypted in-memory store).
use bytes::Bytes;
use futures::StreamExt;
use sqlx::PgPool;
use uuid::Uuid;

use super::{
    files::{FileStorage, NewFile, normalize_content_type, sanitize_filename},
    sweep::{sweep_once, verify},
    *,
};
use crate::store::Store;

struct Fixture {
    pool: PgPool,
    files: FileStorage,
    ws: Uuid,
    other_ws: Uuid,
    user: Uuid,
    key: Uuid,
}

async fn fixture(pool: PgPool) -> Fixture {
    let user = Uuid::new_v4();
    sqlx::query("INSERT INTO users(id,email) VALUES($1,$2)")
        .bind(user)
        .bind(format!("{user}@example.test"))
        .execute(&pool)
        .await
        .unwrap();
    let (ws, other_ws) = (Uuid::new_v4(), Uuid::new_v4());
    for id in [ws, other_ws] {
        sqlx::query("INSERT INTO workspaces(id,name,kind) VALUES($1,'Files','project')")
            .bind(id)
            .execute(&pool)
            .await
            .unwrap();
    }
    let key = Uuid::new_v4();
    sqlx::query("INSERT INTO api_keys(id,workspace_id,issued_to_user_id,name,secret_hash) VALUES($1,$2,$3,'k',decode(repeat('00',32),'hex'))")
        .bind(key)
        .bind(ws)
        .bind(user)
        .execute(&pool)
        .await
        .unwrap();
    Fixture {
        files: FileStorage::new(Store::new(pool.clone()), FileStoreRuntime::memory()),
        pool,
        ws,
        other_ws,
        user,
        key,
    }
}

fn body(data: &[u8]) -> ByteStream {
    futures::stream::iter([Ok(Bytes::copy_from_slice(data))]).boxed()
}

async fn read(mut s: ByteStream) -> Vec<u8> {
    let mut out = Vec::new();
    while let Some(p) = s.next().await {
        out.extend_from_slice(&p.unwrap());
    }
    out
}

async fn enable_batch(pool: &PgPool) {
    sqlx::query("UPDATE installation_settings SET file_batch_enabled=true")
        .execute(pool)
        .await
        .unwrap();
}

#[test]
fn filenames_and_content_types_are_sanitized() {
    assert_eq!(
        sanitize_filename("../../etc/passwd").as_deref(),
        Some("passwd")
    );
    assert_eq!(
        sanitize_filename("C:\\Users\\a\\report.csv").as_deref(),
        Some("report.csv")
    );
    assert_eq!(
        sanitize_filename("in\u{202e}put\n.jsonl").as_deref(),
        Some("input.jsonl")
    );
    assert_eq!(sanitize_filename(" .. "), None);
    assert_eq!(sanitize_filename("dir/"), None);
    assert_eq!(
        sanitize_filename(&"é".repeat(300)).unwrap().chars().count(),
        255
    );
    assert_eq!(
        normalize_content_type("Application/JSONL; charset=utf-8").as_deref(),
        Some("application/jsonl")
    );
    assert_eq!(normalize_content_type("text/html\r\nx: y"), None);
    assert_eq!(normalize_content_type("nonsense"), None);
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn files_are_scoped_toggled_and_tracked(pool: PgPool) {
    let f = fixture(pool).await;
    let mut new = NewFile::new(Purpose::BatchInput, Some(f.ws));
    new.created_by_user_id = Some(f.user);
    new.created_by_api_key_id = Some(f.key);
    new.filename = Some("../../in\u{202e}put.jsonl".into());
    new.content_type = Some("Application/JSONL; charset=utf-8".into());
    // Customer-content purposes start off.
    assert!(!f.files.accepts(Purpose::BatchInput).await.unwrap());
    assert_eq!(
        f.files.create(new.clone(), body(b"x")).await.unwrap_err(),
        FileError::Disabled
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM stored_files")
            .fetch_one(&f.pool)
            .await
            .unwrap(),
        0
    );
    enable_batch(&f.pool).await;
    let file = f
        .files
        .create(new.clone(), body(b"{\"a\":1}\n"))
        .await
        .unwrap();
    assert_eq!(file.filename.as_deref(), Some("input.jsonl"));
    assert_eq!(file.content_type.as_deref(), Some("application/jsonl"));
    assert_eq!(file.size_bytes, 8);
    assert_eq!(file.encryption_key_id, "memory");
    assert_eq!(
        file.key.as_str(),
        format!("batch_input/{}/{}", f.ws, file.id)
    );
    // Default batch retention: 7 days.
    let days = (file.expires_at.unwrap() - file.created_at).num_days();
    assert_eq!(days, 7);
    // Scope is exact: another workspace or installation scope sees nothing.
    assert!(
        f.files
            .get(file.id, Some(f.other_ws))
            .await
            .unwrap()
            .is_none()
    );
    assert!(f.files.get(file.id, None).await.unwrap().is_none());
    let (meta, stream) = f.files.open(file.id, Some(f.ws)).await.unwrap();
    assert_eq!(meta, file);
    assert_eq!(read(stream).await, b"{\"a\":1}\n");
    assert_eq!(f.files.workspace_stored_bytes(f.ws).await.unwrap(), 8);
    assert_eq!(f.files.workspace_stored_bytes(f.other_ws).await.unwrap(), 0);
    // A key from another workspace cannot be recorded as the creator.
    let mut foreign = NewFile::new(Purpose::BatchInput, Some(f.other_ws));
    foreign.created_by_api_key_id = Some(f.key);
    assert_eq!(
        f.files.create(foreign, body(b"x")).await.unwrap_err(),
        FileError::Invalid
    );
    // Branding is installation-wide and follows the backend being on.
    let logo = f
        .files
        .create(NewFile::new(Purpose::Branding, None), body(b"png"))
        .await
        .unwrap();
    assert_eq!(logo.expires_at, None);
    assert!(
        f.files
            .create(NewFile::new(Purpose::Branding, Some(f.ws)), body(b"png"))
            .await
            .is_err()
    );
    // A past explicit expiry is invalid; a near one wins over retention.
    let mut past = NewFile::new(Purpose::Export, None);
    past.expires_at = Some(chrono::Utc::now() - chrono::Duration::seconds(1));
    assert_eq!(
        f.files.create(past, body(b"x")).await.unwrap_err(),
        FileError::Invalid
    );
    let mut soon = NewFile::new(Purpose::Export, Some(f.ws));
    let at = chrono::Utc::now() + chrono::Duration::hours(1);
    soon.expires_at = Some(at);
    let export = f.files.create(soon, body(b"csv")).await.unwrap();
    assert!((export.expires_at.unwrap() - at).num_seconds().abs() <= 1);
    // A failed upload leaves no live row and no object.
    let failing: ByteStream =
        futures::stream::iter([Ok(Bytes::from_static(b"x")), Err(FileStoreError::Source)]).boxed();
    assert_eq!(
        f.files.create(new.clone(), failing).await.unwrap_err(),
        FileError::Store(FileStoreError::Source)
    );
    let abandoned: i64 = sqlx::query_scalar("SELECT count(*) FROM stored_files WHERE deleted_at IS NOT NULL AND committed_at IS NULL AND filename IS NULL").fetch_one(&f.pool).await.unwrap();
    assert_eq!(abandoned, 1);
    // Too large.
    let mut capped = new.clone();
    capped.max_bytes = Some(2);
    assert_eq!(
        f.files.create(capped, body(b"abc")).await.unwrap_err(),
        FileError::Store(FileStoreError::TooLarge)
    );
    // Delete: object gone, row kept with names cleared; second delete is a no-op.
    assert!(f.files.delete(file.id, Some(f.ws)).await.unwrap());
    assert!(!f.files.delete(file.id, Some(f.ws)).await.unwrap());
    assert_eq!(
        f.files
            .runtime()
            .store()
            .unwrap()
            .head(&file.key)
            .await
            .unwrap(),
        None
    );
    let (deleted, name): (bool, Option<String>) =
        sqlx::query_as("SELECT deleted_at IS NOT NULL,filename FROM stored_files WHERE id=$1")
            .bind(file.id)
            .fetch_one(&f.pool)
            .await
            .unwrap();
    assert!(deleted && name.is_none());
    assert_eq!(f.files.workspace_stored_bytes(f.ws).await.unwrap(), 3);
    // Turning the toggle off stops new files.
    sqlx::query("UPDATE installation_settings SET file_batch_enabled=false")
        .execute(&f.pool)
        .await
        .unwrap();
    assert_eq!(
        f.files.create(new, body(b"x")).await.unwrap_err(),
        FileError::Disabled
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn sweeper_honors_per_purpose_retention_and_records_failures(pool: PgPool) {
    let f = fixture(pool).await;
    enable_batch(&f.pool).await;
    let store = f.files.runtime().store().unwrap();
    let batch = f
        .files
        .create(NewFile::new(Purpose::BatchOutput, Some(f.ws)), body(b"out"))
        .await
        .unwrap();
    let export = f
        .files
        .create(NewFile::new(Purpose::Export, None), body(b"csv"))
        .await
        .unwrap();
    let logo = f
        .files
        .create(NewFile::new(Purpose::Branding, None), body(b"png"))
        .await
        .unwrap();
    let age = |id: Uuid, days: i32| {
        let pool = f.pool.clone();
        async move {
            sqlx::query(
                "UPDATE stored_files SET created_at=now()-make_interval(days=>$2) WHERE id=$1",
            )
            .bind(id)
            .bind(days)
            .execute(&pool)
            .await
            .unwrap();
        }
    };
    // Two days old: exports (1 day) expire, batch (7 days) and branding (never) stay.
    for id in [batch.id, export.id, logo.id] {
        age(id, 2).await;
    }
    assert!(
        f.files.get(export.id, None).await.unwrap().is_none(),
        "expired files are unreadable before the sweep"
    );
    let report = sweep_once(&Store::new(f.pool.clone()), f.files.runtime(), 100)
        .await
        .unwrap();
    assert_eq!((report.claimed, report.deleted, report.failed), (1, 1, 0));
    assert_eq!(store.head(&export.key).await.unwrap(), None);
    assert!(store.head(&batch.key).await.unwrap().is_some());
    // Shortening batch retention applies to existing files; branding never expires.
    sqlx::query("UPDATE installation_settings SET file_batch_retention_days=1")
        .execute(&f.pool)
        .await
        .unwrap();
    age(logo.id, 4000).await;
    let report = sweep_once(&Store::new(f.pool.clone()), f.files.runtime(), 100)
        .await
        .unwrap();
    assert_eq!(report.deleted, 1);
    assert!(f.files.get(logo.id, None).await.unwrap().is_some());
    // Abandoned uploads (never committed for a day) are removed.
    let pending = ObjectKey::new(Purpose::UserFile, Scope::Workspace(f.ws)).unwrap();
    sqlx::query("INSERT INTO stored_files(id,object_key,purpose,workspace_id,backend,encryption_key_id,created_at) VALUES($1,$2,'user_file',$3,'memory','memory',now()-interval '2 days')")
        .bind(pending.id()).bind(pending.as_str()).bind(f.ws).execute(&f.pool).await.unwrap();
    // A row recorded under another backend cannot be deleted here: recorded, retried later.
    let elsewhere = ObjectKey::new(Purpose::Export, Scope::Installation).unwrap();
    sqlx::query("INSERT INTO stored_files(id,object_key,purpose,backend,encryption_key_id,size_bytes,sha256,committed_at,expires_at) VALUES($1,$2,'export','s3','memory',1,decode(repeat('00',32),'hex'),now(),now())")
        .bind(elsewhere.id()).bind(elsewhere.as_str()).execute(&f.pool).await.unwrap();
    let db = Store::new(f.pool.clone());
    let report = sweep_once(&db, f.files.runtime(), 100).await.unwrap();
    assert_eq!((report.deleted, report.failed), (1, 1));
    let (attempts, error, deleted): (i32, Option<String>, bool) = sqlx::query_as("SELECT delete_attempts,last_delete_error,deleted_at IS NOT NULL FROM stored_files WHERE id=$1").bind(elsewhere.id()).fetch_one(&f.pool).await.unwrap();
    assert_eq!(
        (attempts, error.as_deref(), deleted),
        (1, Some("backend_mismatch"), false)
    );
    // Claimed rows are leased for 5 minutes: an immediate rerun does not retry.
    assert_eq!(
        sweep_once(&db, f.files.runtime(), 100)
            .await
            .unwrap()
            .claimed,
        0
    );
    // With the store off, nothing is marked deleted.
    sqlx::query("UPDATE stored_files SET last_delete_attempt_at=NULL WHERE deleted_at IS NULL")
        .execute(&f.pool)
        .await
        .unwrap();
    let off = sweep_once(&db, &FileStoreRuntime::off(), 100)
        .await
        .unwrap();
    assert_eq!((off.deleted, off.failed), (0, 1));
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn verify_compares_metadata_with_the_store(pool: PgPool) {
    let f = fixture(pool).await;
    enable_batch(&f.pool).await;
    let db = Store::new(f.pool.clone());
    let a = f
        .files
        .create(NewFile::new(Purpose::BatchInput, Some(f.ws)), body(b"aaaa"))
        .await
        .unwrap();
    let b = f
        .files
        .create(NewFile::new(Purpose::BatchInput, Some(f.ws)), body(b"bb"))
        .await
        .unwrap();
    let report = verify(&db, f.files.runtime(), 1000).await.unwrap();
    assert!(report.consistent(), "{report:?}");
    assert_eq!((report.checked, report.ok), (2, 2));
    assert_eq!(report.by_key_id.get("memory"), Some(&2));
    // An object removed behind the gateway's back, and a recorded size that lies.
    f.files
        .runtime()
        .store()
        .unwrap()
        .delete(&a.key)
        .await
        .unwrap();
    let wrong = ObjectKey::new(Purpose::BatchInput, Scope::Workspace(f.ws)).unwrap();
    sqlx::query("INSERT INTO stored_files(id,object_key,purpose,workspace_id,backend,encryption_key_id,size_bytes,sha256,committed_at) VALUES($1,$2,'batch_input',$3,'memory','retired',99,decode(repeat('00',32),'hex'),now())")
        .bind(wrong.id()).bind(wrong.as_str()).bind(f.ws).execute(&f.pool).await.unwrap();
    f.files
        .runtime()
        .store()
        .unwrap()
        .put(&wrong, body(b"x"), PutMeta::default())
        .await
        .unwrap();
    let report = verify(&db, f.files.runtime(), 1000).await.unwrap();
    assert!(!report.consistent());
    assert_eq!(report.missing, [a.key.to_string()]);
    assert_eq!(report.size_mismatch, [wrong.to_string()]);
    assert_eq!(report.unknown_key_ids, ["retired"]);
    assert_eq!(report.ok, 1);
    let _ = b;
    assert!(verify(&db, &FileStoreRuntime::off(), 10).await.is_err());
}
