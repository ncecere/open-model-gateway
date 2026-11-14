//! Realtime session accounting on a disposable database: composed validators,
//! window holds, extensions under budgets and tokens-per-minute, per-response
//! settlement, unpriced sessions and refused aggregate reconciliation.
use super::tests::db::{Fixture, amounts, fixture};
use super::*;
use crate::inference::{
    realtime::{RealtimeFinish, RealtimeUsage, ResponseBound, ResponseStatus},
    repository::AttemptTelemetry,
};
use serde_json::{Value, json};
use sqlx::PgPool;

fn line(meter: &str, amount: &str) -> Value {
    json!({"meter":meter,"microusd_per_batch":amount,"batch":1000000,"unit_label":"/M tokens","sku_label":"Line"})
}
fn na(meter: &str) -> Value {
    json!({"meter":meter,"not_applicable":true})
}
/// $4/$0.40/$16 text, $32/$0.40/$64 audio per M tokens; other meters NA.
fn lines() -> Value {
    json!([
        line("input_tokens", "4000000"),
        line("cache_read_tokens", "400000"),
        line("output_tokens", "16000000"),
        line("input_audio_tokens", "32000000"),
        line("cache_read_audio_tokens", "400000"),
        line("output_audio_tokens", "64000000"),
        na("cache_write_tokens"), na("cache_write_5m_tokens"), na("cache_write_1h_tokens"),
        na("output_images"), na("input_characters"), na("input_audio_seconds_ms"),
        na("output_audio_seconds_ms"), na("search_units"),
        {"meter":"requests","microusd_per_batch":"0","batch":1,"unit_label":"/request","sku_label":"Request"}
    ])
}
async fn realtime_model(f: &Fixture, price: Option<Value>) -> Deployment {
    sqlx::query("UPDATE models SET supported_protocols=ARRAY['realtime'] WHERE id=$1")
        .bind(f.model)
        .execute(&f.store.pool)
        .await
        .unwrap();
    if let Some(lines) = price {
        sqlx::query("INSERT INTO deployment_prices(id,deployment_id,input_token_limit,output_token_limit,pricing_version,price_lines,max_units) VALUES($1,$2,1000,100,3,$3,'{}')")
            .bind(Uuid::new_v4()).bind(f.deployment).bind(lines).execute(&f.store.pool).await.unwrap();
    }
    use crate::inference::repository::InferenceRepository;
    f.store
        .deployments(&f.principal, "company/smart")
        .await
        .unwrap()
        .remove(0)
}
fn admission(window: u32) -> WorkloadAdmission {
    WorkloadAdmission {
        kind: WorkloadKind::Realtime,
        output: OutputReservation::Requested(Some(window)),
        unit_ceilings: MeterUsage {
            requests: Some(1),
            ..MeterUsage::default()
        },
    }
}
fn finished(id: Uuid, unopened: bool) -> RealtimeFinish {
    RealtimeFinish {
        id,
        outcome: Outcome::Succeeded,
        error: None,
        elapsed_ms: 5,
        telemetry: AttemptTelemetry::default(),
        unopened_request: unopened,
        window: FULL,
    }
}
/// Unknown context: input 1000 (the ceiling) per modality, output 100 →
/// 1000×($4 + $0.40 + $32 + $0.40)/M + 100×($16 + $64)/M = 44,800 µUSD.
const FULL: ResponseBound = ResponseBound {
    text_input: None,
    audio_input: None,
    output: 100,
};
const WINDOW: i64 = 44_800;
/// Admission: 256 text tokens of framing, no audio, output 100 →
/// 1024 + ceil(102.4) + 1600 + 6400 = 9,127 µUSD (356 tokens).
const ADMISSION: i64 = 9_127;
fn admitted() -> ResponseBound {
    ResponseBound::admission(100)
}
/// A context-sized window (text, audio input; output 100).
fn sized(text: u64, audio: u64) -> ResponseBound {
    ResponseBound {
        text_input: Some(text),
        audio_input: Some(audio),
        output: 100,
    }
}
const U: RealtimeUsage = RealtimeUsage {
    input_text_tokens: 10,
    cached_text_tokens: 4,
    input_audio_tokens: 20,
    cached_audio_tokens: 5,
    output_text_tokens: 10,
    output_audio_tokens: 40,
};
const RESPONSE: i64 = 3_228;

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn composed_validators_accept_realtime_shapes_only(pool: PgPool) {
    let valid = |sql: &'static str, v: Value| {
        let pool = pool.clone();
        async move {
            sqlx::query_scalar::<_, bool>(sql)
                .bind(v)
                .fetch_one(&pool)
                .await
                .unwrap()
        }
    };
    let lines_ok = |v: Value| valid("SELECT valid_price_lines($1)", v);
    assert!(lines_ok(lines()).await);
    // Base (0016 and earlier) behaviour is unchanged.
    assert!(lines_ok(json!([na("input_tokens")])).await);
    assert!(!lines_ok(json!([na("bogus")])).await);
    for bad in [
        json!([na("output_audio_tokens")]),
        json!([na("input_tokens"), {"meter":"output_audio_tokens","microusd_per_batch":"1","batch":1000,"unit_label":"/M tokens","sku_label":"A"}]),
        json!([na("input_tokens"), {"meter":"output_audio_tokens","microusd_per_batch":"1","batch":1000000,"unit_label":"/M tokens","sku_label":"A","min_prompt_tokens":10}]),
        json!([
            na("input_tokens"),
            line("output_audio_tokens", "1"),
            line("output_audio_tokens", "2")
        ]),
        json!([
            na("input_tokens"),
            line("output_audio_tokens", "1"),
            na("output_audio_tokens")
        ]),
        json!([na("input_tokens"), {"meter":"output_audio_tokens","not_applicable":true,"sku_label":"x"}]),
        json!([na("bogus"), line("output_audio_tokens", "1")]),
    ] {
        assert!(!lines_ok(bad.clone()).await, "{bad}");
        // Rust validation mirrors the database.
        assert!(
            serde_json::from_value::<PriceLines>(bad.clone()).is_err()
                || bad[0]["meter"] == "bogus",
            "{bad}"
        );
    }
    assert!(serde_json::from_value::<PriceLines>(lines()).is_ok());
    for (protocols, ok) in [
        (vec!["realtime"], true),
        (vec!["realtime", "chat_completions"], false),
        (vec!["realtime", "realtime"], false),
        (vec!["chat_completions", "responses"], true),
        (vec!["embeddings"], true),
    ] {
        let v: bool = sqlx::query_scalar("SELECT valid_model_protocols($1::text[])")
            .bind(&protocols)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(v, ok, "{protocols:?}");
        assert_eq!(ApiProtocol::valid_set(&protocols), ok, "{protocols:?}");
    }
    let mut components = serde_json::to_value(
        CostBreakdown::Realtime(Default::default(), Default::default(), Default::default())
            .to_value(),
    )
    .unwrap();
    assert!(valid("SELECT valid_cost_components($1)", components.clone()).await);
    components["output_audio_tokens_microusd"] = json!("x");
    assert!(!valid("SELECT valid_cost_components($1)", components.clone()).await);
    components
        .as_object_mut()
        .unwrap()
        .remove("output_audio_tokens_microusd");
    assert!(!valid("SELECT valid_cost_components($1)", components).await);
    assert!(
        valid(
            "SELECT valid_cost_components($1)",
            CostBreakdown::Metered(Default::default(), Default::default()).to_value()
        )
        .await
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn windows_extend_settle_and_finish_on_one_reservation(pool: PgPool) {
    let f = fixture(pool).await;
    let d = realtime_model(&f, Some(lines())).await;
    assert_eq!(
        realtime::window_output(&f.store, f.deployment, 4096)
            .await
            .unwrap(),
        100
    );
    assert_eq!(
        realtime::window_output(&f.store, f.deployment, 64)
            .await
            .unwrap(),
        64
    );
    let start = ExecutionStart {
        streamed: true,
        ..f.start()
    };
    admit_workload_for_deployment(&f.store, &start, &admission(100), 960, &d)
        .await
        .unwrap();
    assert_eq!(
        amounts(&f, start.id).await,
        ("pending".into(), Some(ADMISSION), None, false)
    );
    // Response 1 resizes the admission window to its context:
    // 300×4.4 + 100×32.4 + 8000 = 12,560 µUSD.
    realtime::reserve_window(
        &f.store,
        &f.principal,
        start.id,
        "company/smart",
        sized(300, 100),
        Some(admitted()),
    )
    .await
    .unwrap();
    assert_eq!(amounts(&f, start.id).await.1, Some(12_560));
    realtime::open_response(&f.store, start.id, 1, sized(300, 100))
        .await
        .unwrap();
    realtime::settle_response(
        &f.store,
        start.id,
        1,
        Some(ResponseStatus::Completed),
        Some(U),
        sized(300, 100),
    )
    .await
    .unwrap();
    assert_eq!(
        amounts(&f, start.id).await,
        ("pending".into(), Some(RESPONSE), None, false)
    );
    // Response 2 adds a window at the context cap; its usage is unknown and keeps it.
    realtime::reserve_window(
        &f.store,
        &f.principal,
        start.id,
        "company/smart",
        FULL,
        None,
    )
    .await
    .unwrap();
    assert_eq!(amounts(&f, start.id).await.1, Some(RESPONSE + WINDOW));
    realtime::open_response(&f.store, start.id, 2, FULL)
        .await
        .unwrap();
    realtime::settle_response(
        &f.store,
        start.id,
        2,
        Some(ResponseStatus::Completed),
        None,
        FULL,
    )
    .await
    .unwrap();
    // A response cannot settle twice.
    assert!(
        realtime::settle_response(&f.store, start.id, 2, None, Some(U), FULL)
            .await
            .is_err()
    );
    // An extension the client never used is released at the end.
    realtime::reserve_window(
        &f.store,
        &f.principal,
        start.id,
        "company/smart",
        FULL,
        None,
    )
    .await
    .unwrap();
    realtime::finish(&f.store, &finished(start.id, false))
        .await
        .unwrap();
    assert_eq!(
        amounts(&f, start.id).await,
        ("unknown".into(), Some(RESPONSE + WINDOW), None, false)
    );
    let (input, output): (Option<i64>, Option<i64>) =
        sqlx::query_as("SELECT input_tokens,output_tokens FROM inference_executions WHERE id=$1")
            .bind(start.id)
            .fetch_one(&f.store.pool)
            .await
            .unwrap();
    assert_eq!(
        (input, output),
        (None, None),
        "a response with unknown usage keeps totals unknown"
    );
    // Aggregate manual reconciliation cannot reproduce per-response valuation.
    let usage = Usage {
        input_tokens: Some(1),
        output_tokens: Some(1),
        ..Usage::default()
    };
    assert_eq!(
        resolve_usage(
            &f.store,
            f.principal.workspace_id,
            start.id,
            usage,
            "invoice",
            f.owner
        )
        .await,
        Err(InferenceError::InvalidRequest)
    );
    // The totals triggers saw the same holds.
    let totals: (String,) = sqlx::query_as("SELECT (settled_microusd+held_microusd)::text FROM budget_totals WHERE scope_kind='workspace' AND scope_id=$1 AND period='lifetime'").bind(f.principal.workspace_id).fetch_one(&f.store.pool).await.unwrap();
    assert_eq!(totals.0, (RESPONSE + WINDOW).to_string());
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn extensions_respect_budgets_tokens_per_minute_and_live_authorization(pool: PgPool) {
    let f = fixture(pool).await;
    let d = realtime_model(&f, Some(lines())).await;
    let start = ExecutionStart {
        streamed: true,
        ..f.start()
    };
    set_test_budget(
        &f.store.pool,
        "local",
        None,
        Some(f.principal.workspace_id),
        None,
        "month",
        Some(2 * WINDOW),
    )
    .await;
    admit_workload_for_deployment(&f.store, &start, &admission(100), 960, &d)
        .await
        .unwrap();
    realtime::reserve_window(
        &f.store,
        &f.principal,
        start.id,
        "company/smart",
        FULL,
        Some(admitted()),
    )
    .await
    .unwrap();
    realtime::reserve_window(
        &f.store,
        &f.principal,
        start.id,
        "company/smart",
        FULL,
        None,
    )
    .await
    .unwrap();
    assert_eq!(amounts(&f, start.id).await.1, Some(2 * WINDOW));
    assert_eq!(
        realtime::reserve_window(
            &f.store,
            &f.principal,
            start.id,
            "company/smart",
            FULL,
            None
        )
        .await,
        Err(InferenceError::BudgetExceeded(LimitScope::Workspace))
    );
    // Shrinking a reserved window only releases, even with no budget left.
    realtime::reserve_window(
        &f.store,
        &f.principal,
        start.id,
        "company/smart",
        admitted(),
        Some(FULL),
    )
    .await
    .unwrap();
    assert_eq!(amounts(&f, start.id).await.1, Some(WINDOW + ADMISSION));
    realtime::reserve_window(
        &f.store,
        &f.principal,
        start.id,
        "company/smart",
        FULL,
        Some(admitted()),
    )
    .await
    .unwrap();
    set_test_budget(
        &f.store.pool,
        "local",
        None,
        Some(f.principal.workspace_id),
        None,
        "month",
        None,
    )
    .await;
    // Tokens per minute: every window counts in the session's admission minute.
    f.policy("workspace_local_policies", None, Some(3 * 1100), None, None)
        .await;
    realtime::reserve_window(
        &f.store,
        &f.principal,
        start.id,
        "company/smart",
        FULL,
        None,
    )
    .await
    .unwrap();
    assert_eq!(
        realtime::reserve_window(
            &f.store,
            &f.principal,
            start.id,
            "company/smart",
            FULL,
            None
        )
        .await,
        Err(InferenceError::Busy)
    );
    f.policy("workspace_local_policies", None, None, None, None)
        .await;
    // Losing the model ends extensions (already-admitted work may finish).
    sqlx::query("DELETE FROM workspace_model_grants WHERE model_id=$1")
        .bind(f.model)
        .execute(&f.store.pool)
        .await
        .unwrap();
    assert_eq!(
        realtime::reserve_window(
            &f.store,
            &f.principal,
            start.id,
            "company/smart",
            FULL,
            None
        )
        .await,
        Err(InferenceError::ModelUnavailable)
    );
    // An unopened forwarded request keeps its window as unknown.
    realtime::finish(&f.store, &finished(start.id, true))
        .await
        .unwrap();
    let state = amounts(&f, start.id).await;
    assert_eq!((state.0.as_str(), state.1), ("unknown", Some(WINDOW)));
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn pricing_must_be_v3_and_unpriced_sessions_stay_unknown(pool: PgPool) {
    let f = fixture(pool).await;
    // Unpriced: admitted without budgets, every response unknown and unbounded.
    let d = realtime_model(&f, None).await;
    let start = ExecutionStart {
        streamed: true,
        ..f.start()
    };
    admit_workload_for_deployment(&f.store, &start, &admission(100), 960, &d)
        .await
        .unwrap();
    realtime::open_response(&f.store, start.id, 1, admitted())
        .await
        .unwrap();
    realtime::settle_response(
        &f.store,
        start.id,
        1,
        Some(ResponseStatus::Completed),
        Some(U),
        admitted(),
    )
    .await
    .unwrap();
    realtime::finish(&f.store, &finished(start.id, false))
        .await
        .unwrap();
    assert_eq!(
        amounts(&f, start.id).await,
        ("unknown".into(), None, None, true)
    );
    // Zero responses settle at a known zero.
    let empty = ExecutionStart {
        streamed: true,
        ..f.start()
    };
    admit_workload_for_deployment(&f.store, &empty, &admission(100), 960, &d)
        .await
        .unwrap();
    realtime::finish(&f.store, &finished(empty.id, false))
        .await
        .unwrap();
    assert_eq!(amounts(&f, empty.id).await.2, Some(0));
    // v1 prices cannot value audio tokens.
    f.price(1).await;
    let v1 = ExecutionStart {
        streamed: true,
        ..f.start()
    };
    assert_eq!(
        admit_workload_for_deployment(&f.store, &v1, &admission(50), 960, &d).await,
        Err(InferenceError::Configuration)
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn lease_expiry_closes_open_responses_and_keeps_their_windows(pool: PgPool) {
    let f = fixture(pool).await;
    let d = realtime_model(&f, Some(lines())).await;
    let start = ExecutionStart {
        streamed: true,
        ..f.start()
    };
    admit_workload_for_deployment(&f.store, &start, &admission(100), 960, &d)
        .await
        .unwrap();
    realtime::open_response(&f.store, start.id, 1, admitted())
        .await
        .unwrap();
    sqlx::query("UPDATE governance_reservations SET lease_expires_at=now()-interval '1 second' WHERE execution_id=$1")
        .bind(start.id).execute(&f.store.pool).await.unwrap();
    assert_eq!(reconcile_expired(&f.store, 10).await.unwrap(), 1);
    assert_eq!(
        amounts(&f, start.id).await,
        ("unknown".into(), Some(ADMISSION), None, false)
    );
    let state: String =
        sqlx::query_scalar("SELECT state FROM realtime_responses WHERE execution_id=$1")
            .bind(start.id)
            .fetch_one(&f.store.pool)
            .await
            .unwrap();
    assert_eq!(state, "unknown");
    // A late finish never overwrites the reconciled session.
    assert!(
        realtime::finish(&f.store, &finished(start.id, false))
            .await
            .is_err()
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn context_sized_windows_settle_known_and_flag_usage_above_them(pool: PgPool) {
    let f = fixture(pool).await;
    let d = realtime_model(&f, Some(lines())).await;
    let start = ExecutionStart {
        streamed: true,
        ..f.start()
    };
    admit_workload_for_deployment(&f.store, &start, &admission(100), 960, &d)
        .await
        .unwrap();
    let reserved = |id| {
        let pool = f.store.pool.clone();
        async move {
            sqlx::query_scalar::<_, Option<i64>>(
                "SELECT reserved_tokens FROM governance_reservations WHERE execution_id=$1",
            )
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap()
        }
    };
    assert_eq!(reserved(start.id).await, Some(256 + 100));
    // Within its context-sized window: known, bounded.
    let w = sized(20, 25);
    realtime::reserve_window(
        &f.store,
        &f.principal,
        start.id,
        "company/smart",
        w,
        Some(admitted()),
    )
    .await
    .unwrap();
    assert_eq!(reserved(start.id).await, Some(45 + 100));
    realtime::open_response(&f.store, start.id, 1, w)
        .await
        .unwrap();
    realtime::settle_response(
        &f.store,
        start.id,
        1,
        Some(ResponseStatus::Completed),
        Some(U),
        w,
    )
    .await
    .unwrap();
    assert_eq!(
        amounts(&f, start.id).await,
        ("pending".into(), Some(RESPONSE), None, false)
    );
    // Audio input above the window's audio bound: valued, but unbounded.
    let small = sized(20, 19);
    realtime::reserve_window(
        &f.store,
        &f.principal,
        start.id,
        "company/smart",
        small,
        None,
    )
    .await
    .unwrap();
    realtime::open_response(&f.store, start.id, 2, small)
        .await
        .unwrap();
    realtime::settle_response(
        &f.store,
        start.id,
        2,
        Some(ResponseStatus::Completed),
        Some(U),
        small,
    )
    .await
    .unwrap();
    realtime::finish(&f.store, &finished(start.id, false))
        .await
        .unwrap();
    assert_eq!(
        amounts(&f, start.id).await,
        (
            "settled".into(),
            Some(2 * RESPONSE),
            Some(2 * RESPONSE),
            true
        )
    );
}

/// #497: an adapter that connects a realtime session with an idle upstream.
struct IdleRealtime;
#[async_trait::async_trait]
impl crate::providers::ProviderAdapter for IdleRealtime {
    fn id(&self) -> &'static str {
        "openai"
    }
    fn capabilities(&self) -> crate::inference::types::Capabilities {
        crate::inference::types::Capabilities {
            text_chat: false,
            streaming: false,
            tools: false,
        }
    }
    fn supports_protocol(&self, protocol: ApiProtocol) -> bool {
        protocol == ApiProtocol::Realtime
    }
    async fn execute(
        &self,
        _: &Deployment,
        _: ChatRequest,
    ) -> Result<crate::inference::types::ProviderOutput, InferenceError> {
        Err(InferenceError::Unsupported)
    }
    async fn connect_realtime(
        &self,
        _: &Deployment,
        _: &crate::inference::realtime::RealtimeSetup,
    ) -> Result<crate::inference::realtime::RealtimeUpstream, InferenceError> {
        use futures_util::SinkExt;
        Ok(crate::inference::realtime::RealtimeUpstream {
            sink: Box::pin(
                futures_util::sink::drain::<String>()
                    .sink_map_err(|never: std::convert::Infallible| match never {}),
            ),
            events: Box::pin(futures_util::stream::pending()),
            config: Default::default(),
        })
    }
}

/// The exported count of one (stage, cause) series of the process-wide metrics.
async fn exported(stage: &str, cause: &str) -> u64 {
    let exposition = crate::metrics::METRICS.render(None).await;
    crate::db::accounting_failure::tests::exported_series(&exposition)
        .into_iter()
        .find(|(s, c, _)| (s.as_str(), c.as_str()) == (stage, cause))
        .map_or(0, |(_, _, n)| n)
}

/// #497: each realtime accounting step whose own deadline elapses (window
/// reservation 5 s; response open, response settlement and session finish
/// 3 s; the detached finish of a dropped session 3 s) is counted once at the
/// `realtime_deadline` stage, naming what it waited on: the in-process gate,
/// a pooled connection, or the database. Before, none was counted.
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn elapsed_realtime_accounting_deadlines_name_what_they_waited_on(pool: PgPool) {
    use crate::db::{
        accounting_failure::tests::{labels, signal_lines},
        diagnostics::{self, Handle},
    };
    use crate::inference::{Engine, EngineLimits, realtime::RealtimeLimits};
    use sqlx::Connection;
    use std::time::Duration;
    let f = fixture(pool.clone()).await;
    realtime_model(&f, Some(lines())).await;
    // Two connections, and an acquire timeout longer than every deadline.
    let store = Store::new(
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .acquire_timeout(Duration::from_secs(20))
            .connect_with((*pool.connect_options()).clone())
            .await
            .unwrap(),
    );
    let mut registry = crate::providers::ProviderRegistry::default();
    registry.register(std::sync::Arc::new(IdleRealtime)).unwrap();
    let engine = Engine::new(
        std::sync::Arc::new(store.clone()),
        registry,
        EngineLimits::default(),
    )
    .unwrap()
    .with_realtime_limits(RealtimeLimits {
        max_output_tokens: 100,
        ..RealtimeLimits::default()
    })
    .unwrap();
    let open = || engine.open_realtime(f.principal, "company/smart", Uuid::new_v4());
    let lock = || async {
        let mut blocker = sqlx::PgConnection::connect_with(&pool.connect_options())
            .await
            .unwrap();
        sqlx::raw_sql("BEGIN; LOCK TABLE governance_reservations IN ACCESS EXCLUSIVE MODE")
            .execute(&mut blocker)
            .await
            .unwrap();
        blocker
    };
    let unlock = |mut blocker: sqlx::PgConnection| async move {
        sqlx::query("ROLLBACK").execute(&mut blocker).await.unwrap();
        blocker.close().await.unwrap();
    };
    let gate = &store.lock_gates.settlement;
    let mut session = open().await.unwrap();

    // Opening a response waits for its session's reservation row.
    let blocker = lock().await;
    let handle = Handle::new();
    let opened = diagnostics::scope(handle.clone(), session.open_response("r1".into())).await;
    unlock(blocker).await;
    assert_eq!(opened, Err(InferenceError::Storage));
    assert_eq!(
        labels(&handle.signals()),
        [("realtime_deadline", "deadline_in_database")],
        "the response open"
    );
    session.open_response("r1".into()).await.unwrap();

    // Settling it waits at the settlement gate (every permit taken).
    let permits = gate.acquire_many(gate.available_permits() as u32).await;
    let handle = Handle::new();
    let settled = diagnostics::scope(
        handle.clone(),
        session.settle_response("r1", Some(ResponseStatus::Completed), None),
    )
    .await;
    drop(permits);
    assert_eq!(settled, Err(InferenceError::Storage));
    assert_eq!(
        labels(&handle.signals()),
        [("realtime_deadline", "deadline_in_queue")],
        "the response settlement"
    );
    session
        .settle_response("r1", Some(ResponseStatus::Completed), None)
        .await
        .unwrap();

    // The next window waits for a pooled connection (both are busy).
    let held = [
        store.pool.acquire().await.unwrap(),
        store.pool.acquire().await.unwrap(),
    ];
    let handle = Handle::new();
    let reserved = diagnostics::scope(handle.clone(), session.reserve_window(50)).await;
    drop(held);
    assert_eq!(reserved, Err(InferenceError::Storage));
    assert_eq!(
        labels(&handle.signals()),
        [("realtime_deadline", "deadline_acquiring_connection")],
        "the window reservation"
    );

    // The session finish waits for its reservation row.
    let blocker = lock().await;
    let handle = Handle::new();
    let finished = diagnostics::scope(
        handle.clone(),
        session.finish(Outcome::Cancelled, None, None),
    )
    .await;
    unlock(blocker).await;
    assert_eq!(finished, Err(InferenceError::Storage));
    assert_eq!(
        labels(&handle.signals()),
        [("realtime_deadline", "deadline_in_database")],
        "the session finish"
    );

    // A dropped session finishes in a detached task: no request capture
    // reaches it, so the process-wide counter and the log show it. No other
    // test produces this pair.
    let dropped = open().await.unwrap();
    let capture = crate::log_capture_test::Capture::start();
    let pair = ("realtime_deadline", "deadline_in_queue");
    let before = exported(pair.0, pair.1).await;
    let permits = gate.acquire_many(gate.available_permits() as u32).await;
    drop(dropped);
    let mut after = before;
    for _ in 0..200 {
        after = exported(pair.0, pair.1).await;
        if after > before {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    drop(permits);
    assert_eq!(after, before + 1, "the detached finish is counted once");
    let logged = signal_lines(&capture.text())
        .into_iter()
        .filter(|(s, c)| (s.as_str(), c.as_str()) == pair)
        .count();
    assert_eq!(logged, 1, "and logged once");
}
