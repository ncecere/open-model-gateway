//! Real engine/HTTP/PostgreSQL timeout, health and retained-hold evidence.
use super::*;
use crate::{
    governance::tests::db::{Fixture, fixture},
    inference::{
        Engine, EngineLimits,
        admission_validation_tests::{add_deployment, issue_key, serve},
    },
    providers::ProviderRegistry,
};
use sqlx::PgPool;
use uuid::Uuid;

type ExecutionRow = (
    String,
    Option<String>,
    Option<i64>,
    Option<i64>,
    String,
    Option<i64>,
    Option<i64>,
);

struct World {
    f: Fixture,
    deployment: Uuid,
    engine: Engine,
    mock: TimedMock,
    token: String,
}
async fn world(pool: PgPool, script: Script, deadline: Duration) -> World {
    world_with_timeouts(pool, script, deadline, timeouts()).await
}
async fn world_with_timeouts(
    pool: PgPool,
    script: Script,
    deadline: Duration,
    timeouts: transport::Timeouts,
) -> World {
    let mut f = fixture(pool).await;
    let token = issue_key(&mut f).await;
    let deployment = add_deployment(
        &f,
        "bedrock",
        "aws:default",
        Some("us-east-1"),
        "anthropic.test",
        0,
    )
    .await;
    let mock = TimedMock::new(script).await;
    let mut registry = ProviderRegistry::default();
    registry
        .register(Arc::new(BedrockAdapter::for_test_with_timeouts(
            &mock.base, timeouts,
        )))
        .unwrap();
    let engine = Engine::new(
        Arc::new(f.store.clone()),
        registry,
        EngineLimits {
            request_timeout: deadline,
            ..EngineLimits::default()
        },
    )
    .unwrap();
    World {
        f,
        deployment,
        engine,
        mock,
        token,
    }
}
impl World {
    async fn stream(&self, id: Uuid) -> EventStream {
        let mut req = request();
        req.model = "company/smart".into();
        req.max_output_tokens = Some(16);
        req.stream = true;
        let ProviderOutput::Stream(output) = self
            .engine
            .execute(self.f.principal, req, id)
            .await
            .unwrap()
        else {
            panic!("expected stream")
        };
        output
    }
    async fn http(
        &self,
    ) -> (
        reqwest::Response,
        crate::inference::admission_validation_tests::Server,
    ) {
        let app = crate::http::router_with_engine(self.f.store.clone(), None, self.engine.clone());
        let (base, server) = serve(app).await;
        let response = reqwest::Client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .post(format!("{base}/v1/chat/completions"))
            .bearer_auth(&self.token)
            .json(
                &json!({"model":"company/smart","stream":true,"max_tokens":16,
                "messages":[{"role":"user","content":"hi"}]}),
            )
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        (response, server)
    }
    async fn recorded(&self, id: Uuid, state: &str, error: Option<&str>, health: Option<i32>) {
        // Cancellation finalization is asynchronous; wait only for this request's row.
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let found: Option<String> = sqlx::query_scalar(
                    "SELECT state FROM inference_executions WHERE root_request_id=$1",
                )
                .bind(id)
                .fetch_optional(&self.f.store.pool)
                .await
                .unwrap();
                if found.as_deref() == Some(state) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("attempt finalized");
        let (actual_state, code, input, output, reservation, held, actual): ExecutionRow =
            sqlx::query_as("SELECT e.state,e.error_code,e.input_tokens,e.output_tokens,r.state,r.held_microusd,r.actual_microusd FROM inference_executions e JOIN governance_reservations r ON r.execution_id=e.id WHERE e.root_request_id=$1")
                .bind(id).fetch_one(&self.f.store.pool).await.unwrap();
        assert_eq!(actual_state, state);
        assert_eq!(code.as_deref(), error);
        let failures: Option<i32> = sqlx::query_scalar(
            "SELECT consecutive_failures FROM deployment_health WHERE deployment_id=$1",
        )
        .bind(self.deployment)
        .fetch_optional(&self.f.store.pool)
        .await
        .unwrap();
        assert_eq!(
            failures, health,
            "neutral cancellations must not write a health observation"
        );
        let ledger: Vec<String> = sqlx::query_scalar("SELECT l.kind FROM monetary_ledger l JOIN inference_executions e ON l.execution_id=e.id WHERE e.root_request_id=$1 ORDER BY kind")
            .bind(id).fetch_all(&self.f.store.pool).await.unwrap();
        if state == "succeeded" {
            assert_eq!((input, output, actual), (Some(3), Some(5), Some(8)));
            assert_eq!(reservation, "settled");
            assert_eq!(ledger, ["hold", "settlement"]);
        } else {
            assert_eq!((input, output, actual), (None, None, None));
            assert_eq!(reservation, "unknown");
            assert!(held.is_some_and(|held| held > 0));
            assert_eq!(ledger, ["hold", "unknown"]);
        }
        let attempts: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM inference_executions WHERE root_request_id=$1",
        )
        .bind(id)
        .fetch_one(&self.f.store.pool)
        .await
        .unwrap();
        assert_eq!(attempts, 1, "no stream retry or failover");
    }
}
fn response_id(response: &reqwest::Response) -> Uuid {
    response.headers()["x-request-id"]
        .to_str()
        .unwrap()
        .parse()
        .unwrap()
}

#[dbtest::test]
async fn healthy_http_stream_beyond_old_cap_records_success(pool: PgPool) {
    let w = world(pool, Script::Healthy, Duration::from_secs(5)).await;
    let start = Instant::now();
    let (response, _server) = w.http().await;
    let id = response_id(&response);
    let body = response.text().await.unwrap();
    assert!(start.elapsed() > TOTAL);
    assert!(
        body.contains("[DONE]") && !body.contains("\"error\""),
        "{body}"
    );
    w.recorded(id, "succeeded", None, Some(0)).await;
}

#[dbtest::test]
async fn paused_engine_reader_beyond_both_timers_records_success(pool: PgPool) {
    let w = world(pool, Script::Gated, Duration::from_secs(5)).await;
    let id = Uuid::now_v7();
    let mut output = w.stream(id).await;
    first(&mut output).await;
    tokio::time::sleep(PAUSE).await;
    w.mock.gate.notify_one();
    done(&mut output).await;
    w.recorded(id, "succeeded", None, Some(0)).await;
}

#[dbtest::test]
async fn active_http_read_stall_records_failure_and_retains_hold(pool: PgPool) {
    let mut w = world(pool, Script::Stalled, Duration::from_secs(5)).await;
    let (response, _server) = w.http().await;
    let id = response_id(&response);
    let body = response.text().await.unwrap();
    assert!(
        body.contains("Hello") && body.contains("timeout_error") && !body.contains("[DONE]"),
        "{body}"
    );
    w.mock.cancelled().await;
    w.recorded(id, "failed", Some("timeout_error"), Some(1))
        .await;
}

// An unpolled reader must not keep transport alive after either a shorter or
// longer engine deadline. Upon resuming, timeout is health-neutral, with no usage.
async fn deadline(pool: PgPool, duration: Duration) {
    let mut w = world(pool, Script::Gated, duration).await;
    let id = Uuid::now_v7();
    let mut output = w.stream(id).await;
    first(&mut output).await;
    w.mock.cancelled().await; // watchdog closes it, with no downstream poll
    assert!(matches!(
        output.next().await,
        Some(Err(InferenceError::Timeout))
    ));
    drop(output);
    w.recorded(id, "failed", Some("timeout_error"), None).await;
}
#[dbtest::test]
async fn engine_deadline_shorter_than_old_cap_is_neutral(pool: PgPool) {
    deadline(pool, Duration::from_millis(350)).await;
}
#[dbtest::test]
async fn engine_deadline_longer_than_old_cap_is_neutral(pool: PgPool) {
    deadline(pool, Duration::from_millis(900)).await;
}

#[dbtest::test]
async fn http_client_disconnect_is_neutral_and_retains_unknown_hold(pool: PgPool) {
    let mut w = world(pool, Script::Stalled, Duration::from_secs(5)).await;
    let (mut response, _server) = w.http().await;
    let id = response_id(&response);
    // Wait for a real content event, then disconnect while upstream is pending.
    while !String::from_utf8_lossy(&response.chunk().await.unwrap().unwrap()).contains("Hello") {}
    drop(response);
    w.mock.cancelled().await;
    w.recorded(id, "cancelled", None, None).await;
}

async fn active_deadline(
    pool: PgPool,
    script: Script,
    duration: Duration,
    timeouts: transport::Timeouts,
) {
    let mut w = world_with_timeouts(pool, script, duration, timeouts).await;
    let (response, _server) = w.http().await;
    let id = response_id(&response);
    let body_started = Instant::now();
    let body = response.text().await.unwrap();
    if matches!(script, Script::Flowing) {
        assert!(body_started.elapsed() > timeouts.total);
    } else {
        assert!(duration < timeouts.read);
    }
    assert!(
        body.contains("Hello") && body.contains("timeout_error") && !body.contains("[DONE]"),
        "{body}"
    );
    w.mock.cancelled().await;
    w.recorded(id, "failed", Some("timeout_error"), None).await;
}
#[dbtest::test]
async fn active_engine_deadline_before_read_timer_is_neutral(pool: PgPool) {
    // Allow real database admission and SDK headers to finish before testing
    // the returned stream. The engine deadline still precedes the read bound.
    active_deadline(
        pool,
        Script::Stalled,
        Duration::from_secs(2),
        transport::Timeouts {
            read: Duration::from_secs(3),
            total: Duration::from_secs(10),
        },
    )
    .await;
}
#[dbtest::test]
async fn active_engine_deadline_after_old_total_cap_is_neutral(pool: PgPool) {
    active_deadline(pool, Script::Flowing, Duration::from_secs(2), timeouts()).await;
}

#[dbtest::test]
async fn healthy_frames_queued_during_pending_reader_pause_record_success(pool: PgPool) {
    let w = world(pool, Script::Gated, Duration::from_secs(5)).await;
    let id = Uuid::now_v7();
    let mut output = w.stream(id).await;
    first(&mut output).await;
    assert!(futures_util::poll!(output.next()).is_pending());
    w.mock.gate.notify_one();
    tokio::time::sleep(PAUSE).await;
    done(&mut output).await;
    w.recorded(id, "succeeded", None, Some(0)).await;
}

#[dbtest::test]
async fn partial_frame_backpressure_stays_healthy_and_settles(pool: PgPool) {
    let w = world(pool, Script::Fragmented, Duration::from_secs(5)).await;
    let id = Uuid::now_v7();
    let mut output = w.stream(id).await;
    partial_resume(&w.mock, &mut output).await;
    w.recorded(id, "succeeded", None, Some(0)).await;
}
