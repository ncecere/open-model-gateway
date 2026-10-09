//! Non-generation workload admission and settlement on a disposable database:
//! rerank and System One with v3 meter bounds, request-derived ceilings,
//! budget denial and input-only enforcement.
use super::tests::db::{Fixture, amounts, fixture};
use super::*;
use crate::{
    billing::v3::Meter, inference::error::LimitScope, inference::repository::InferenceRepository,
    providers::metering,
};
use serde_json::{Value, json};
use sqlx::PgPool;

fn line(meter: Meter, amount: &str, batch: u64) -> Value {
    json!({"meter":meter.as_str(),"microusd_per_batch":amount,"batch":batch,"unit_label":meter.unit_label(batch).unwrap(),"sku_label":"Line"})
}
fn na(meter: Meter) -> Value {
    json!({"meter":meter.as_str(),"not_applicable":true})
}
/// Input tokens 1 µUSD/token; caches, images, audio, characters not applicable.
fn lines(output: Value, search: Value, requests: Value) -> Vec<Value> {
    vec![
        line(Meter::InputTokens, "1000000", 1_000_000),
        output,
        na(Meter::CacheReadTokens),
        na(Meter::CacheWriteTokens),
        na(Meter::CacheWrite5mTokens),
        na(Meter::CacheWrite1hTokens),
        na(Meter::OutputImages),
        na(Meter::InputCharacters),
        na(Meter::InputAudioSecondsMs),
        na(Meter::OutputAudioSecondsMs),
        search,
        requests,
    ]
}
async fn price(f: &Fixture, lines: Vec<Value>, max: Value) {
    sqlx::query("INSERT INTO deployment_prices(id,deployment_id,input_token_limit,output_token_limit,pricing_version,price_lines,max_units) VALUES($1,$2,100,50,3,$3,$4)").bind(Uuid::new_v4()).bind(f.deployment).bind(json!(lines)).bind(max).execute(&f.store.pool).await.unwrap();
}
async fn protocols(f: &Fixture, protocol: &str) -> Deployment {
    sqlx::query("UPDATE models SET supported_protocols=ARRAY[$2] WHERE id=$1")
        .bind(f.model)
        .bind(protocol)
        .execute(&f.store.pool)
        .await
        .unwrap();
    f.store
        .deployments(&f.principal, "company/smart")
        .await
        .unwrap()
        .remove(0)
}
async fn budget(f: &Fixture, amount: Option<i64>) {
    crate::governance::set_test_budget(
        &f.store.pool,
        "local",
        None,
        Some(f.principal.workspace_id),
        None,
        "month",
        amount,
    )
    .await;
}
fn rerank_admission() -> WorkloadAdmission {
    WorkloadAdmission {
        kind: WorkloadKind::Rerank,
        output: OutputReservation::None,
        unit_ceilings: MeterUsage {
            requests: Some(1),
            ..MeterUsage::default()
        },
    }
}
fn systemone_admission() -> WorkloadAdmission {
    WorkloadAdmission {
        kind: WorkloadKind::Systemone,
        output: OutputReservation::PriceCeiling,
        unit_ceilings: rerank_admission().unit_ceilings,
    }
}
fn finished(id: Uuid, usage: Usage) -> ExecutionFinish {
    ExecutionFinish {
        id,
        outcome: Outcome::Succeeded,
        error: None,
        usage,
        elapsed_ms: 5,
    }
}
fn rerank_usage(tokens: u64, search_units: Option<u64>) -> Usage {
    let mut usage = metering::input_only(Some(tokens));
    usage.meters = Some(metering::text_workload_meters(search_units));
    usage.provider_cost_microusd = Some(2000);
    usage
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn rerank_v3_hold_uses_max_units_and_request_ceiling_then_settles_exactly(pool: PgPool) {
    let f = fixture(pool).await;
    let deployment = protocols(&f, "rerank").await;
    // $0.002/search (ceiling 1 from max_units), $0.0001/request (no
    // max_units: bounded by the request-derived ceiling of one request).
    price(
        &f,
        lines(
            na(Meter::OutputTokens),
            line(Meter::SearchUnits, "2000", 1),
            line(Meter::Requests, "100", 1),
        ),
        json!({"search_units":"1"}),
    )
    .await;
    // Hold = 100 input tokens + 2000 + 100 = 2200: exactly fits the budget.
    budget(&f, Some(2200)).await;
    let start = f.start();
    admit_workload_for_deployment(&f.store, &start, &rerank_admission(), 30, &deployment)
        .await
        .unwrap();
    assert_eq!(
        amounts(&f, start.id).await,
        ("pending".into(), Some(2200), None, false)
    );
    let kind: (String, i64) = sqlx::query_as("SELECT e.workload_kind,r.reserved_tokens FROM inference_executions e JOIN governance_reservations r ON r.execution_id=e.id WHERE e.id=$1").bind(start.id).fetch_one(&f.store.pool).await.unwrap();
    assert_eq!(kind, ("rerank".into(), 100));
    finish(&f.store, &finished(start.id, rerank_usage(25, Some(1))))
        .await
        .unwrap();
    assert_eq!(amounts(&f, start.id).await.2, Some(25 + 2000 + 100));
    let row: (Value, Value, Option<i64>, i64) = sqlx::query_as("SELECT cost_components,meter_usage,provider_cost_microusd,output_tokens FROM governance_reservations WHERE execution_id=$1").bind(start.id).fetch_one(&f.store.pool).await.unwrap();
    assert_eq!(row.0["search_units_microusd"], "2000");
    assert_eq!(row.0["requests_microusd"], "100");
    assert_eq!(row.0["uncached_input_microusd"], "25");
    assert_eq!(row.1["requests"], "1");
    assert_eq!(
        row.2,
        Some(2000),
        "provider cost is evidence, not the charge"
    );
    assert_eq!(row.3, 0);
    // The settled 2125 leaves 75 µUSD: the next 2200 hold is denied.
    assert_eq!(
        admit_workload_for_deployment(&f.store, &f.start(), &rerank_admission(), 30, &deployment)
            .await,
        Err(InferenceError::BudgetExceeded(LimitScope::Workspace))
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn rerank_unbounded_search_units_deny_budget_and_unknown_count_keeps_floor(pool: PgPool) {
    let f = fixture(pool).await;
    let deployment = protocols(&f, "rerank").await;
    price(
        &f,
        lines(
            na(Meter::OutputTokens),
            line(Meter::SearchUnits, "2000", 1),
            line(Meter::Requests, "0", 1),
        ),
        json!({}),
    )
    .await;
    budget(&f, Some(1_000_000_000)).await;
    assert_eq!(
        admit_workload_for_deployment(&f.store, &f.start(), &rerank_admission(), 30, &deployment)
            .await,
        Err(InferenceError::PriceUnbounded)
    );
    budget(&f, None).await;
    let start = f.start();
    admit_workload_for_deployment(&f.store, &start, &rerank_admission(), 30, &deployment)
        .await
        .unwrap();
    assert_eq!(
        amounts(&f, start.id).await,
        ("pending".into(), None, None, true)
    );
    // Search units not reported: unknown, never free; known tokens are a floor.
    finish(&f.store, &finished(start.id, rerank_usage(25, None)))
        .await
        .unwrap();
    assert_eq!(
        amounts(&f, start.id).await,
        ("unknown".into(), Some(25), None, true)
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn rerank_not_applicable_search_units_settle_and_output_must_stay_zero(pool: PgPool) {
    let f = fixture(pool).await;
    let deployment = protocols(&f, "rerank").await;
    price(
        &f,
        lines(
            na(Meter::OutputTokens),
            na(Meter::SearchUnits),
            line(Meter::Requests, "0", 1),
        ),
        json!({}),
    )
    .await;
    budget(&f, Some(100)).await;
    let start = f.start();
    admit_workload_for_deployment(&f.store, &start, &rerank_admission(), 30, &deployment)
        .await
        .unwrap();
    assert_eq!(amounts(&f, start.id).await.1, Some(100));
    let mut bad = rerank_usage(25, None);
    bad.output_tokens = Some(3);
    assert_eq!(
        finish(&f.store, &finished(start.id, bad)).await,
        Err(InferenceError::Storage)
    );
    finish(&f.store, &finished(start.id, rerank_usage(25, None)))
        .await
        .unwrap();
    assert_eq!(amounts(&f, start.id).await.2, Some(25));
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn systemone_reserves_price_output_ceiling_and_free_output_settles(pool: PgPool) {
    let f = fixture(pool).await;
    let deployment = protocols(&f, "systemone").await;
    let free = lines(
        line(Meter::OutputTokens, "0", 1_000_000),
        na(Meter::SearchUnits),
        line(Meter::Requests, "0", 1),
    );
    price(&f, free, json!({})).await;
    let start = f.start();
    admit_workload_for_deployment(&f.store, &start, &systemone_admission(), 30, &deployment)
        .await
        .unwrap();
    // 100 input tokens; output is reserved to the price ceiling (50) but free.
    let held: (Option<i64>, i64) = sqlx::query_as(
        "SELECT held_microusd,reserved_tokens FROM governance_reservations WHERE execution_id=$1",
    )
    .bind(start.id)
    .fetch_one(&f.store.pool)
    .await
    .unwrap();
    assert_eq!(held, (Some(100), 150));
    let mut usage = metering::input_only(Some(100));
    usage.output_tokens = Some(21);
    usage.meters = Some(metering::text_workload_meters(Some(0)));
    finish(&f.store, &finished(start.id, usage)).await.unwrap();
    assert_eq!(
        amounts(&f, start.id).await,
        ("settled".into(), Some(100), Some(100), false)
    );
    // A priced output meter is bounded by the trusted output ceiling.
    let priced = lines(
        line(Meter::OutputTokens, "2000000", 1_000_000),
        na(Meter::SearchUnits),
        line(Meter::Requests, "0", 1),
    );
    price(&f, priced, json!({})).await;
    let start = f.start();
    admit_workload_for_deployment(&f.store, &start, &systemone_admission(), 30, &deployment)
        .await
        .unwrap();
    assert_eq!(amounts(&f, start.id).await.1, Some(100 + 100));
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn workload_admission_requires_a_matching_model_protocol(pool: PgPool) {
    let f = fixture(pool).await;
    let deployment = protocols(&f, "rerank").await;
    assert_eq!(
        admit_workload_for_deployment(
            &f.store,
            &f.start(),
            &systemone_admission(),
            30,
            &deployment
        )
        .await,
        Err(InferenceError::Configuration)
    );
    let generation = WorkloadAdmission {
        kind: WorkloadKind::Generation,
        output: OutputReservation::Requested(Some(1)),
        unit_ceilings: MeterUsage::default(),
    };
    assert_eq!(
        admit_workload_for_deployment(&f.store, &f.start(), &generation, 30, &deployment).await,
        Err(InferenceError::Configuration)
    );
    // The live deployment row must still match what routing selected.
    let stale = protocols(&f, "chat_completions").await;
    assert_eq!(
        admit_workload_for_deployment(&f.store, &f.start(), &rerank_admission(), 30, &stale).await,
        Err(InferenceError::Configuration)
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM inference_executions")
            .fetch_one(&f.store.pool)
            .await
            .unwrap(),
        0
    );
}
