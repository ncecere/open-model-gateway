//! A provider answer is charged once across either side of the result commit.
use super::*;
use super::{
    fence::{lapse, start},
    http_support::{Resolver, Server, client},
};
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Clone, Copy)]
enum Loss {
    Crash,
    Takeover,
}
#[derive(Clone, Copy)]
enum Boundary {
    BeforeCommit,
    AfterCommit,
}

// A fresh worker process reopens the same encrypted files and disposable database.
async fn local_files(dir: &std::path::Path) -> FileStoreRuntime {
    use base64::Engine as _;
    let config = crate::filestore::FileStoreConfig::from_lookup(&|key| match key {
        "GATEWAY_FILE_STORE" => Some("local".into()),
        "GATEWAY_FILE_STORE_DIR" => Some(dir.join("files").to_str().unwrap().into()),
        "GATEWAY_FILE_ENCRYPTION_KEYS_ENV" => Some("TEST_KEY".into()),
        "TEST_KEY" => Some(format!(
            "test:{}",
            base64::engine::general_purpose::STANDARD
                .encode(std::array::from_fn::<u8, 32, _>(|i| i as u8))
        )),
        _ => None,
    })
    .unwrap();
    FileStoreRuntime::build(config).await.unwrap()
}

fn loopback_engine(store: &crate::store::Store, base: &str) -> Engine {
    let mut registry = ProviderRegistry::default();
    registry
        .register(Arc::new(crate::providers::openai::OpenAiAdapter::for_test(
            Arc::new(Resolver),
            format!("{base}/v1"),
        )))
        .unwrap();
    Engine::new(Arc::new(store.clone()), registry, EngineLimits::default()).unwrap()
}

#[tokio::test]
#[ignore = "child process of the batch crash tests"]
async fn crash_worker() {
    let dir = std::env::var("BATCH_TEST_DIR").unwrap();
    let db = std::env::var("BATCH_TEST_DB").unwrap();
    assert!(db.starts_with("_sqlx_test_"));
    let options = std::env::var("DATABASE_URL")
        .unwrap()
        .parse::<sqlx::postgres::PgConnectOptions>()
        .unwrap()
        .database(&db);
    let pool = PgPool::connect_with(options).await.unwrap();
    let store = crate::store::Store::new(pool);
    let engine = loopback_engine(&store, &std::env::var("BATCH_TEST_UPSTREAM").unwrap());
    let jobs = Jobs::new(store, &engine).with_files(Some(local_files(dir.as_ref()).await));
    jobs.finalize_hold.arm();
    assert!(Runner::new(jobs).run_next().await.unwrap());
}

fn start_process(w: &World, dir: &std::path::Path, upstream: &Server) -> tokio::process::Child {
    let module = module_path!().split_once("::").unwrap().1;
    tokio::process::Command::new(std::env::current_exe().unwrap())
        .args(["--ignored", "--exact", &format!("{module}::crash_worker")])
        .env(
            "BATCH_TEST_DB",
            w.f.store.pool.connect_options().get_database().unwrap(),
        )
        .env("BATCH_TEST_DIR", dir)
        .env("BATCH_TEST_UPSTREAM", &upstream.base)
        .kill_on_drop(true)
        .spawn()
        .unwrap()
}

async fn run_case(pool: PgPool, fault: Option<(Boundary, Loss)>) {
    let size = if fault.is_some() { 1 } else { 32 };
    let before = matches!(fault, Some((Boundary::BeforeCommit, _)));
    let mut w = world(pool, false, false, 1).await;
    let dir = tempfile::tempdir().unwrap();
    let crash = matches!(fault, Some((_, Loss::Crash)));
    if crash {
        let runtime = local_files(dir.path()).await;
        w.jobs = w.jobs.with_files(Some(runtime.clone()));
        w.files = crate::filestore::FileStorage::new(w.f.store.clone(), runtime);
    }
    let input: Vec<_> = (0..size)
        .map(|n| chat_line(&n.to_string(), "company/smart", "test"))
        .collect();
    let file = w.upload(&w.f.principal, &input).await;
    let calls = Arc::new(AtomicUsize::default());
    let seen = calls.clone();
    let upstream = Server::start(axum::Router::new().fallback(move || {
        seen.fetch_add(1, Ordering::SeqCst);
        async { axum::Json(json!({"id":"chatcmpl-test","model":"smart","choices":[{"index":0,"message":{"role":"assistant","content":"answer"},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}})) }
    })).await;
    let engine = loopback_engine(&w.f.store, &upstream.base);
    let gateway = Server::start(
        crate::http::router_with_engine(w.f.store.clone(), None, engine.clone())
            .layer(axum::Extension(w.files.runtime().clone())),
    )
    .await;
    let auth = crate::jobs::db_tests::bearer(&w.f, w.f.principal.workspace_id).await;
    let client = client();
    let created = client.post(format!("{}/v1/batches", gateway.base)).header("authorization", &auth)
        .json(&json!({"input_file_id":file,"endpoint":"/v1/chat/completions","completion_window":"24h","metadata":{"omg_mode":"gateway"}}))
        .send().await.unwrap().error_for_status().unwrap().json::<Value>().await.unwrap();
    let id = created["id"].as_str().unwrap();
    let job = crate::jobs::store::reload(
        &w.f.store,
        Uuid::parse_str(id.strip_prefix("batch_").unwrap()).unwrap(),
    )
    .await
    .unwrap();
    w.jobs = w.jobs.with_engine(engine);
    let mut lock = w.f.store.pool.begin().await.unwrap();
    if before {
        sqlx::query("LOCK TABLE stored_files IN SHARE MODE")
            .execute(&mut *lock)
            .await
            .unwrap();
    } else if fault.is_some() && !crash {
        w.jobs.finalize_hold.arm();
    }
    let started = std::time::Instant::now();
    let mut process = crash.then(|| start_process(&w, dir.path(), &upstream));
    let old = (!crash).then(|| start(&w));
    if let Some((_, loss)) = fault {
        if before {
            tokio::time::timeout(Duration::from_secs(20),async {
                loop {
                    let waiting:bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_locks WHERE relation='stored_files'::regclass AND database=(SELECT oid FROM pg_database WHERE datname=current_database()) AND NOT granted)").fetch_one(&w.f.store.pool).await.unwrap();
                    if waiting {break}
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }).await.unwrap();
        } else if crash {
            tokio::time::timeout(Duration::from_secs(20), async {
                while w.reload(&job).await.finalizing_at.is_none() {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
        } else {
            tokio::time::timeout(Duration::from_secs(20), w.jobs.finalize_hold.reached())
                .await
                .unwrap();
        }
        let row:(String,Option<i32>,i32) = sqlx::query_as("SELECT l.state,l.segment,j.request_completed FROM batch_lines l JOIN async_jobs j ON j.id=l.job_id WHERE j.id=$1").bind(job.id).fetch_one(&w.f.store.pool).await.unwrap();
        let cost = crate::governance::batch::batch_cost(&w.f.store, job.id)
            .await
            .unwrap();
        assert_eq!((cost.0, cost.1), (15, 0));
        if before {
            assert_eq!(row, ("running".into(), None, 0));
        } else {
            assert_eq!(row, ("succeeded".into(), Some(0), 1));
        }
        if let Loss::Crash = loss {
            let child = process.as_mut().unwrap();
            child.kill().await.unwrap();
            assert!(!child.wait().await.unwrap().success());
        }
        lapse(&w, &job).await;
        let owner = start(&w);
        tokio::time::timeout(Duration::from_secs(10), async {
            while sqlx::query_scalar::<_, i64>("SELECT runner_epoch FROM async_jobs WHERE id=$1")
                .bind(job.id)
                .fetch_one(&w.f.store.pool)
                .await
                .unwrap()
                == 1
            {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        lock.rollback().await.unwrap();
        w.jobs.finalize_hold.release();
        if let Some(old) = old {
            assert!(old.await.unwrap().unwrap());
        }
        assert!(owner.await.unwrap().unwrap());
    } else {
        lock.rollback().await.unwrap();
        assert!(old.unwrap().await.unwrap().unwrap());
        eprintln!("32 serial lines took {:?}", started.elapsed());
    }
    assert_eq!(calls.load(Ordering::SeqCst), size);
    let done = w.reload(&job).await;
    let cost = crate::governance::batch::batch_cost(&w.f.store, job.id)
        .await
        .unwrap();
    assert_eq!((cost.0, cost.1), (15 * size as i64, 0));
    let failed = i32::from(before);
    assert_eq!(
        (done.request_completed, done.request_failed),
        (Some(size as i32 - failed), Some(failed))
    );
    let status = client
        .get(format!("{}/v1/batches/{id}", gateway.base))
        .header("authorization", &auth)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(status["status"], "completed");
    let file = status[if before {
        "error_file_id"
    } else {
        "output_file_id"
    }]
    .as_str()
    .unwrap();
    let content = client
        .get(format!("{}/v1/files/{file}/content", gateway.base))
        .header("authorization", &auth)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .text()
        .await
        .unwrap();
    let records: Vec<Value> = content
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(records.len(), size);
    let ids: std::collections::HashSet<_> = records
        .iter()
        .map(|r| r["custom_id"].as_str().unwrap())
        .collect();
    for n in 0..size {
        assert!(ids.contains(n.to_string().as_str()));
    }
    if before {
        assert_eq!(records[0]["error"]["code"], "result_unavailable");
    } else {
        for record in records {
            assert_eq!(
                record["response"]["body"]["choices"][0]["message"]["content"],
                "answer"
            );
        }
    }
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn crash_before(pool: PgPool) {
    run_case(pool, Some((Boundary::BeforeCommit, Loss::Crash))).await;
}
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn takeover_before(pool: PgPool) {
    run_case(pool, Some((Boundary::BeforeCommit, Loss::Takeover))).await;
}
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn crash_after(pool: PgPool) {
    run_case(pool, Some((Boundary::AfterCommit, Loss::Crash))).await;
}
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn takeover_after(pool: PgPool) {
    run_case(pool, Some((Boundary::AfterCommit, Loss::Takeover))).await;
}
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn serial_results(pool: PgPool) {
    run_case(pool, None).await;
}
