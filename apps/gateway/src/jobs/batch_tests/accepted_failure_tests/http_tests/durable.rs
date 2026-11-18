//! A provider answer is charged once across either side of the result commit.
use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

async fn run_case(pool: PgPool, before: bool, crash: bool, size: usize) {
    let mut w = world(pool, false, false, 1).await;
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
    let mut registry = ProviderRegistry::default();
    registry
        .register(Arc::new(crate::providers::openai::OpenAiAdapter::for_test(
            Arc::new(Resolver),
            format!("{}/v1", upstream.base),
        )))
        .unwrap();
    let engine = Engine::new(
        Arc::new(w.f.store.clone()),
        registry,
        EngineLimits::default(),
    )
    .unwrap();
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
    } else if size == 1 {
        w.jobs.finalize_hold.arm();
    }
    let started = std::time::Instant::now();
    let old = super::super::super::fence::start(&w);
    if size == 1 {
        if before {
            tokio::time::timeout(Duration::from_secs(20),async {
                loop {
                    let waiting:bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_locks WHERE relation='stored_files'::regclass AND database=(SELECT oid FROM pg_database WHERE datname=current_database()) AND NOT granted)").fetch_one(&w.f.store.pool).await.unwrap();
                    if waiting {break}
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }).await.unwrap();
        } else {
            w.jobs.finalize_hold.reached().await;
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
        if crash {
            old.abort();
        }
        super::super::super::fence::lapse(&w, &job).await;
        let owner = super::super::super::fence::start(&w);
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
        if crash {
            assert!(old.await.unwrap_err().is_cancelled());
        } else {
            assert!(old.await.unwrap().unwrap());
        }
        assert!(owner.await.unwrap().unwrap());
    } else {
        lock.rollback().await.unwrap();
        assert!(old.await.unwrap().unwrap());
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
    run_case(pool, true, true, 1).await;
}
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn takeover_before(pool: PgPool) {
    run_case(pool, true, false, 1).await;
}
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn crash_after(pool: PgPool) {
    run_case(pool, false, true, 1).await;
}
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn takeover_after(pool: PgPool) {
    run_case(pool, false, false, 1).await;
}
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn serial_results(pool: PgPool) {
    run_case(pool, false, false, 32).await;
}
