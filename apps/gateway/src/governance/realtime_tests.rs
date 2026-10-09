//! Realtime session accounting on a disposable database: composed validators,
//! window holds, extensions under budgets and tokens-per-minute, per-response
//! settlement, unpriced sessions and refused aggregate reconciliation.
use super::tests::db::{Fixture, amounts, fixture};
use super::*;
use crate::inference::{
    realtime::{RealtimeFinish, RealtimeUsage, ResponseStatus},
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
        window_output_tokens: 100,
    }
}
const WINDOW: i64 = 44_800;
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
        ("pending".into(), Some(WINDOW), None, false)
    );
    // Response 1 uses the admission window.
    realtime::open_response(&f.store, start.id, 1, 100)
        .await
        .unwrap();
    realtime::settle_response(
        &f.store,
        start.id,
        1,
        Some(ResponseStatus::Completed),
        Some(U),
        100,
    )
    .await
    .unwrap();
    assert_eq!(
        amounts(&f, start.id).await,
        ("pending".into(), Some(RESPONSE), None, false)
    );
    // Response 2 extends by one window; its usage is unknown and keeps it.
    realtime::reserve_window(&f.store, &f.principal, start.id, "company/smart", 100)
        .await
        .unwrap();
    assert_eq!(amounts(&f, start.id).await.1, Some(RESPONSE + WINDOW));
    realtime::open_response(&f.store, start.id, 2, 100)
        .await
        .unwrap();
    realtime::settle_response(
        &f.store,
        start.id,
        2,
        Some(ResponseStatus::Completed),
        None,
        100,
    )
    .await
    .unwrap();
    // A response cannot settle twice.
    assert!(
        realtime::settle_response(&f.store, start.id, 2, None, Some(U), 100)
            .await
            .is_err()
    );
    // An extension the client never used is released at the end.
    realtime::reserve_window(&f.store, &f.principal, start.id, "company/smart", 100)
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
    realtime::reserve_window(&f.store, &f.principal, start.id, "company/smart", 100)
        .await
        .unwrap();
    assert_eq!(
        realtime::reserve_window(&f.store, &f.principal, start.id, "company/smart", 100).await,
        Err(InferenceError::BudgetExceeded(LimitScope::Workspace))
    );
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
    realtime::reserve_window(&f.store, &f.principal, start.id, "company/smart", 100)
        .await
        .unwrap();
    assert_eq!(
        realtime::reserve_window(&f.store, &f.principal, start.id, "company/smart", 100).await,
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
        realtime::reserve_window(&f.store, &f.principal, start.id, "company/smart", 100).await,
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
    realtime::open_response(&f.store, start.id, 1, 100)
        .await
        .unwrap();
    realtime::settle_response(
        &f.store,
        start.id,
        1,
        Some(ResponseStatus::Completed),
        Some(U),
        100,
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
    realtime::open_response(&f.store, start.id, 1, 100)
        .await
        .unwrap();
    sqlx::query("UPDATE governance_reservations SET lease_expires_at=now()-interval '1 second' WHERE execution_id=$1")
        .bind(start.id).execute(&f.store.pool).await.unwrap();
    assert_eq!(reconcile_expired(&f.store, 10).await.unwrap(), 1);
    assert_eq!(
        amounts(&f, start.id).await,
        ("unknown".into(), Some(WINDOW), None, false)
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
