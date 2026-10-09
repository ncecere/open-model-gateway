//! Pricing v3 on a disposable database: immutability, exact charges, admission
//! bounds, budget denial and meter evidence through reconciliation.
use super::tests::db::{Fixture, amounts, billing, done, fixture, request};
use super::*;
use crate::{
    billing::v3::Meter,
    inference::{error::LimitScope, repository::InferenceRepository},
};
use serde_json::{Value, json};
use sqlx::PgPool;

fn line(meter: Meter, amount: &str, batch: u64) -> Value {
    json!({"meter":meter.as_str(),"microusd_per_batch":amount,"batch":batch,"unit_label":meter.unit_label(batch).unwrap(),"sku_label":"Line"})
}
fn na(meter: Meter) -> Value {
    json!({"meter":meter.as_str(),"not_applicable":true})
}
/// Token lines matching the v2 fixture rates plus a >50-token input tier.
fn token_lines() -> Vec<Value> {
    let mut tier = line(Meter::InputTokens, "2000000", 1_000_000);
    tier["min_prompt_tokens"] = json!(50);
    vec![
        line(Meter::InputTokens, "1000000", 1_000_000),
        tier,
        line(Meter::OutputTokens, "3000000", 1_000_000),
        line(Meter::CacheReadTokens, "100000", 1_000_000),
        line(Meter::CacheWriteTokens, "1250000", 1_000_000),
        line(Meter::CacheWrite5mTokens, "1250000", 1_000_000),
        line(Meter::CacheWrite1hTokens, "2000000", 1_000_000),
        na(Meter::OutputImages),
        na(Meter::InputCharacters),
        na(Meter::InputAudioSecondsMs),
        na(Meter::OutputAudioSecondsMs),
        na(Meter::SearchUnits),
        line(Meter::Requests, "0", 1),
    ]
}
async fn v3(f: &Fixture, lines: Vec<Value>, max: Value) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO deployment_prices(id,deployment_id,input_token_limit,output_token_limit,pricing_version,price_lines,max_units) VALUES($1,$2,100,50,3,$3,$4)").bind(id).bind(f.deployment).bind(json!(lines)).bind(max).execute(&f.store.pool).await.unwrap();
    id
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

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn v3_shape_is_validated_in_sql_and_immutable(pool: PgPool) {
    let f = fixture(pool).await;
    let id = v3(&f, token_lines(), json!({})).await;
    for sql in [
        "UPDATE deployment_prices SET price_lines='[]' WHERE id=$1",
        "UPDATE deployment_prices SET max_units='{\"requests\":\"9\"}' WHERE id=$1",
        "DELETE FROM deployment_prices WHERE id=$1",
    ] {
        assert!(
            sqlx::query(sql)
                .bind(id)
                .execute(&f.store.pool)
                .await
                .is_err(),
            "{sql}"
        );
    }
    let valid = |v: Value| {
        let pool = f.store.pool.clone();
        async move {
            sqlx::query_scalar::<_, bool>("SELECT valid_price_lines($1)")
                .bind(v)
                .fetch_one(&pool)
                .await
                .unwrap()
        }
    };
    assert!(valid(json!(token_lines())).await);
    let base = line(Meter::OutputImages, "20500", 1);
    let mut bad_variant = line(Meter::InputTokens, "1", 1_000_000);
    bad_variant["variant"] = json!("1K");
    let mut bad_tier = line(Meter::InputTokens, "1", 1_000_000);
    bad_tier["min_prompt_tokens"] = json!(0);
    let mut bad_label = line(Meter::InputTokens, "1", 1_000_000);
    bad_label["unit_label"] = json!("/image");
    for bad in [
        json!([]),
        json!([{"meter":"bogus","not_applicable":true}]),
        json!([{"meter":"requests","not_applicable":false}]),
        json!([line(Meter::OutputImages, "1.5", 1)]),
        json!([line(Meter::OutputImages, "-1", 1)]),
        json!([{"meter":"output_images","microusd_per_batch":"1","batch":2,"unit_label":"/image","sku_label":"I"}]),
        json!([bad_variant]),
        json!([bad_tier]),
        json!([bad_label]),
        json!([base, base]),
        json!([base, na(Meter::OutputImages)]),
    ] {
        assert!(!valid(bad.clone()).await, "{bad}");
        assert!(
            serde_json::from_value::<PriceLines>(bad.clone()).is_err(),
            "Rust/SQL parity {bad}"
        );
    }
    // Version shapes stay exclusive: v3 has no scalar rates or cache_pricing.
    assert!(sqlx::query("INSERT INTO deployment_prices(id,deployment_id,input_microusd_per_million,output_microusd_per_million,input_token_limit,output_token_limit,pricing_version,price_lines,max_units) VALUES($1,$2,1,1,100,50,3,$3,'{}')").bind(Uuid::new_v4()).bind(f.deployment).bind(json!(token_lines())).execute(&f.store.pool).await.is_err());
    assert!(sqlx::query("INSERT INTO deployment_prices(id,deployment_id,input_microusd_per_million,output_microusd_per_million,input_token_limit,output_token_limit,pricing_version,price_lines) VALUES($1,$2,1,1,100,50,1,$3)").bind(Uuid::new_v4()).bind(f.deployment).bind(json!(token_lines())).execute(&f.store.pool).await.is_err());
    assert!(sqlx::query("INSERT INTO deployment_prices(id,deployment_id,input_token_limit,output_token_limit,pricing_version,price_lines,max_units) VALUES($1,$2,100,50,3,$3,'{\"input_tokens\":\"1\"}')").bind(Uuid::new_v4()).bind(f.deployment).bind(json!(token_lines())).execute(&f.store.pool).await.is_err());
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn v3_exact_tiered_settlement_and_pinned_bound(pool: PgPool) {
    let f = fixture(pool).await;
    let price = v3(&f, token_lines(), json!({})).await;
    let start = f.start();
    admit(&f.store, &start, &request(), 30).await.unwrap();
    // 200 input tier (100 > 50) + 10 read + 125 + 125 + 200 writes + 30 output.
    assert_eq!(
        amounts(&f, start.id).await,
        ("pending".into(), Some(690), None, false)
    );
    let mut record = done(start.id, Some(30), Some(2));
    record.usage.billing = Some(billing());
    finish(&f.store, &record).await.unwrap();
    assert_eq!(amounts(&f, start.id).await.2, Some(42));
    let components: Value = sqlx::query_scalar(
        "SELECT cost_components FROM governance_reservations WHERE execution_id=$1",
    )
    .bind(start.id)
    .fetch_one(&f.store.pool)
    .await
    .unwrap();
    assert_eq!(components.as_object().unwrap().len(), 12);
    assert_eq!(components["uncached_input_microusd"], "10");
    assert_eq!(components["requests_microusd"], "0");
    // Prompt 60 > 50 selects the tier: 40 uncached at 2 µUSD/token.
    let start = f.start();
    admit(&f.store, &start, &request(), 30).await.unwrap();
    let mut record = done(start.id, Some(60), Some(2));
    record.usage.billing = Some(BillingUsage {
        total_input_tokens: Some(60),
        uncached_input_tokens: Some(40),
        ..billing()
    });
    finish(&f.store, &record).await.unwrap();
    assert_eq!(amounts(&f, start.id).await.2, Some(80 + 1 + 4 + 5 + 16 + 6));
    // Later versions never reprice pinned attempts.
    let pinned: Option<Uuid> =
        sqlx::query_scalar("SELECT price_id FROM governance_reservations WHERE execution_id=$1")
            .bind(start.id)
            .fetch_one(&f.store.pool)
            .await
            .unwrap();
    assert_eq!(pinned, Some(price));
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn v3_unknown_meter_keeps_floor_and_unbounded_budget_is_denied(pool: PgPool) {
    let f = fixture(pool).await;
    let mut lines = token_lines();
    lines.retain(|l| l["meter"] != "output_tokens");
    v3(&f, lines, json!({})).await;
    budget(&f, Some(1_000_000)).await;
    assert_eq!(
        admit(&f.store, &f.start(), &request(), 30).await,
        Err(InferenceError::PriceUnbounded)
    );
    budget(&f, None).await;
    let start = f.start();
    admit(&f.store, &start, &request(), 30).await.unwrap();
    assert_eq!(
        amounts(&f, start.id).await,
        ("pending".into(), None, None, true)
    );
    let mut record = done(start.id, Some(30), Some(2));
    record.usage.billing = Some(billing());
    finish(&f.store, &record).await.unwrap();
    // Known token charges (36) are a floor; output is unknown, not free.
    assert_eq!(
        amounts(&f, start.id).await,
        ("unknown".into(), Some(36), None, true)
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn v3_unit_meter_needs_trusted_ceiling_for_budget(pool: PgPool) {
    let f = fixture(pool).await;
    let mut lines = token_lines();
    lines.retain(|l| l["meter"] != "input_characters");
    lines.push(line(Meter::InputCharacters, "15000000", 1_000_000));
    v3(&f, lines.clone(), json!({})).await;
    budget(&f, Some(1_000_000_000)).await;
    assert_eq!(
        admit(&f.store, &f.start(), &request(), 30).await,
        Err(InferenceError::PriceUnbounded)
    );
    // With a trusted ceiling the hold is finite: 690 + ceil(4096 × 15).
    v3(&f, lines, json!({"input_characters":"4096"})).await;
    let start = f.start();
    admit(&f.store, &start, &request(), 30).await.unwrap();
    assert_eq!(amounts(&f, start.id).await.1, Some(690 + 61_440));
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn v3_meter_evidence_variants_and_reconciliation(pool: PgPool) {
    let f = fixture(pool).await;
    let mut image_768 = line(Meter::OutputImages, "20500", 1);
    image_768["variant"] = json!("768");
    let mut image_4k = line(Meter::OutputImages, "303500", 1);
    image_4k["variant"] = json!("4K");
    let mut lines: Vec<Value> = Meter::ALL
        .into_iter()
        .filter(|m| !matches!(m, Meter::OutputImages | Meter::Requests))
        .map(na)
        .collect();
    lines.extend([image_768, image_4k, line(Meter::Requests, "1000", 1)]);
    v3(&f, lines, json!({"output_images":"4","requests":"1"})).await;
    budget(&f, Some(10_000_000)).await;
    let start = f.start();
    admit(&f.store, &start, &request(), 30).await.unwrap();
    assert_eq!(amounts(&f, start.id).await.1, Some(4 * 303_500 + 1000));
    let meters = MeterUsage {
        output_images: Some(2),
        requests: Some(1),
        ..Default::default()
    };
    let usage = Usage {
        input_tokens: Some(0),
        output_tokens: Some(0),
        meters: Some(meters),
        output_image_variant: MeterVariant::new("768"),
        provider_cost_microusd: Some(41_000),
        ..Default::default()
    };
    // Failed attempts never settle, but keep their meter evidence and hold.
    let mut failed = done(start.id, None, None);
    failed.outcome = Outcome::Failed;
    failed.error = Some(InferenceError::UpstreamUnavailable);
    failed.usage = Usage {
        input_tokens: None,
        output_tokens: None,
        ..usage
    };
    finish(&f.store, &failed).await.unwrap();
    assert_eq!(
        amounts(&f, start.id).await,
        ("unknown".into(), Some(4 * 303_500 + 1000), None, false)
    );
    let stored: (Value, String, i64) = sqlx::query_as("SELECT meter_usage,output_image_variant,provider_cost_microusd FROM inference_executions WHERE id=$1").bind(start.id).fetch_one(&f.store.pool).await.unwrap();
    assert_eq!(stored.0["output_images"], "2");
    assert_eq!(stored.0["search_units"], Value::Null);
    assert_eq!((stored.1.as_str(), stored.2), ("768", 41_000));
    // Erasing or changing meter evidence fails closed.
    for bad in [
        Usage {
            meters: None,
            ..usage
        },
        Usage {
            output_image_variant: MeterVariant::new("4K"),
            ..usage
        },
        Usage {
            provider_cost_microusd: Some(1),
            ..usage
        },
        Usage {
            meters: Some(MeterUsage {
                output_images: Some(1),
                ..meters
            }),
            ..usage
        },
    ] {
        assert_eq!(
            resolve_usage(
                &f.store,
                f.principal.workspace_id,
                start.id,
                bad,
                "provider-ref",
                f.owner
            )
            .await,
            Err(InferenceError::InvalidRequest)
        );
    }
    resolve_usage(
        &f.store,
        f.principal.workspace_id,
        start.id,
        usage,
        "provider-ref",
        f.owner,
    )
    .await
    .unwrap();
    // 2 × $0.0205 + $0.001 request fee.
    assert_eq!(amounts(&f, start.id).await.2, Some(42_000));
    resolve_usage(
        &f.store,
        f.principal.workspace_id,
        start.id,
        usage,
        "provider-ref",
        f.owner,
    )
    .await
    .unwrap();
    let ledger: (Value, Value, String) = sqlx::query_as("SELECT meter_usage,cost_components,output_image_variant FROM monetary_ledger WHERE execution_id=$1 AND kind='reconciliation'").bind(start.id).fetch_one(&f.store.pool).await.unwrap();
    assert_eq!(ledger.0["requests"], "1");
    assert_eq!(ledger.1["output_images_microusd"], "41000");
    assert_eq!(ledger.2, "768");
    // An unpriced variant cannot settle and invalidates the hold as a bound.
    let start = f.start();
    admit(&f.store, &start, &request(), 30).await.unwrap();
    let mut record = done(start.id, Some(0), Some(0));
    record.usage = Usage {
        output_image_variant: MeterVariant::new("2K"),
        ..usage
    };
    finish(&f.store, &record).await.unwrap();
    let (state, _, actual, unbounded) = amounts(&f, start.id).await;
    assert_eq!((state.as_str(), actual, unbounded), ("unknown", None, true));
    // Over-ceiling usage settles exactly but marks the hold as exceeded.
    let start = f.start();
    admit(&f.store, &start, &request(), 30)
        .await
        .expect_err("unbounded history now blocks the workspace budget");
    budget(&f, None).await;
    admit(&f.store, &start, &request(), 30).await.unwrap();
    let mut record = done(start.id, Some(0), Some(0));
    record.usage = Usage {
        meters: Some(MeterUsage {
            output_images: Some(5),
            ..meters
        }),
        ..usage
    };
    finish(&f.store, &record).await.unwrap();
    assert_eq!(amounts(&f, start.id).await.2, Some(5 * 20_500 + 1000));
}

/// Every meter explicitly free or not applicable (e.g. a `:free` rerank model).
fn free_lines() -> Vec<Value> {
    Meter::ALL
        .into_iter()
        .map(|m| match m {
            Meter::InputTokens | Meter::OutputTokens | Meter::Requests | Meter::SearchUnits => {
                line(m, "0", m.batches()[0])
            }
            m => na(m),
        })
        .collect()
}
fn rejected(id: Uuid, error: InferenceError) -> ExecutionFinish {
    let mut record = done(id, None, None);
    record.outcome = Outcome::Failed;
    record.error = Some(error);
    record
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn free_price_pre_processing_rejection_settles_known_zero(pool: PgPool) {
    let f = fixture(pool).await;
    v3(&f, free_lines(), json!({})).await;
    let start = f.start();
    admit(&f.store, &start, &request(), 30).await.unwrap();
    let record = rejected(start.id, InferenceError::UpstreamRejected);
    finish(&f.store, &record).await.unwrap();
    assert_eq!(
        amounts(&f, start.id).await,
        ("settled".into(), Some(0), Some(0), false)
    );
    let (state, error, input, output): (String, Option<String>, Option<i64>, Option<i64>) =
        sqlx::query_as(
            "SELECT state,error_code,input_tokens,output_tokens FROM inference_executions WHERE id=$1",
        )
        .bind(start.id)
        .fetch_one(&f.store.pool)
        .await
        .unwrap();
    assert_eq!(
        (state.as_str(), error.as_deref(), input, output),
        ("failed", Some("upstream_rejected"), Some(0), Some(0))
    );
    // An identical retry of the finish is idempotent.
    finish(&f.store, &record).await.unwrap();
    let ledger: (String, Option<i64>) = sqlx::query_as(
        "SELECT kind,amount_microusd FROM monetary_ledger WHERE execution_id=$1 AND kind<>'hold'",
    )
    .bind(start.id)
    .fetch_one(&f.store.pool)
    .await
    .unwrap();
    assert_eq!(ledger, ("settlement".into(), Some(0)));
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn other_failures_partial_usage_or_paid_meters_stay_unknown(pool: PgPool) {
    let f = fixture(pool).await;
    v3(&f, free_lines(), json!({})).await;
    let unknown = |f: &Fixture, id| {
        let f = f.store.pool.clone();
        async move {
            sqlx::query_as::<_, (String, Option<i64>)>(
                "SELECT state,actual_microusd FROM governance_reservations WHERE execution_id=$1",
            )
            .bind(id)
            .fetch_one(&f)
            .await
            .unwrap()
        }
    };
    // Not a pre-processing rejection: transport/server failure, timeout, cancel.
    for error in [
        InferenceError::UpstreamUnavailable,
        InferenceError::Timeout,
        InferenceError::InvalidUpstream,
    ] {
        let start = f.start();
        admit(&f.store, &start, &request(), 30).await.unwrap();
        finish(&f.store, &rejected(start.id, error)).await.unwrap();
        assert_eq!(
            unknown(&f, start.id).await,
            ("unknown".into(), None),
            "{error:?}"
        );
    }
    let start = f.start();
    admit(&f.store, &start, &request(), 30).await.unwrap();
    let mut cancelled = rejected(start.id, InferenceError::UpstreamRejected);
    cancelled.outcome = Outcome::Cancelled;
    finish(&f.store, &cancelled).await.unwrap();
    assert_eq!(unknown(&f, start.id).await.0, "unknown");
    // Any reported usage or nonzero provider cost keeps it unknown.
    for usage in [
        Usage {
            output_tokens: Some(3),
            ..Default::default()
        },
        Usage {
            meters: Some(MeterUsage {
                requests: Some(1),
                ..Default::default()
            }),
            ..Default::default()
        },
        Usage {
            provider_cost_microusd: Some(5),
            ..Default::default()
        },
    ] {
        let start = f.start();
        admit(&f.store, &start, &request(), 30).await.unwrap();
        let mut record = rejected(start.id, InferenceError::UpstreamRejected);
        record.usage = usage;
        finish(&f.store, &record).await.unwrap();
        assert_eq!(unknown(&f, start.id).await.0, "unknown", "{usage:?}");
    }
    // A single paid or missing meter, or a v1/v2 price, keeps it unknown.
    let mut paid = free_lines();
    paid.retain(|l| l["meter"] != "requests");
    paid.push(line(Meter::Requests, "1", 1));
    let mut missing = free_lines();
    missing.retain(|l| l["meter"] != "search_units");
    for lines in [paid, missing] {
        v3(&f, lines, json!({"requests":"1"})).await;
        let start = f.start();
        admit(&f.store, &start, &request(), 30).await.unwrap();
        finish(
            &f.store,
            &rejected(start.id, InferenceError::UpstreamRejected),
        )
        .await
        .unwrap();
        assert_eq!(unknown(&f, start.id).await.0, "unknown");
    }
    f.price(0).await;
    let start = f.start();
    admit(&f.store, &start, &request(), 30).await.unwrap();
    finish(
        &f.store,
        &rejected(start.id, InferenceError::UpstreamRejected),
    )
    .await
    .unwrap();
    assert_eq!(unknown(&f, start.id).await.0, "unknown");
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn token_reservation_above_tokens_per_minute_is_a_distinct_denial(pool: PgPool) {
    let f = fixture(pool).await;
    // The "minute's tokens are used" step needs both admissions in one minute.
    f.store.freeze_admission_clock().await.unwrap();
    // Reservation = input ceiling 100 + requested output 10 = 110 tokens.
    v3(&f, token_lines(), json!({})).await;
    f.policy("workspace_local_policies", None, Some(109), None, None)
        .await;
    assert_eq!(
        admit(&f.store, &f.start(), &request(), 30).await,
        Err(InferenceError::TokenReservationExceedsLimit(
            LimitScope::Workspace
        ))
    );
    f.policy("workspace_local_policies", None, None, None, None)
        .await;
    f.policy("installation_policy", None, Some(100), None, None)
        .await;
    assert_eq!(
        admit(&f.store, &f.start(), &request(), 30).await,
        Err(InferenceError::TokenReservationExceedsLimit(
            LimitScope::Installation
        ))
    );
    // A reservation that fits stays an ordinary, retryable rate limit once
    // the minute's tokens are used.
    f.policy("installation_policy", None, Some(110), None, None)
        .await;
    admit(&f.store, &f.start(), &request(), 30).await.unwrap();
    assert_eq!(
        admit(&f.store, &f.start(), &request(), 30).await,
        Err(InferenceError::Busy)
    );
    // Budget denials keep precedence at the narrower scope.
    f.policy("installation_policy", None, Some(1), None, None)
        .await;
    budget(&f, Some(1)).await;
    assert_eq!(
        admit(&f.store, &f.start(), &request(), 30).await,
        Err(InferenceError::BudgetExceeded(LimitScope::Workspace))
    );
    // No reservation was recorded for any denial.
    let reservations: i64 = sqlx::query_scalar("SELECT count(*) FROM governance_reservations")
        .fetch_one(&f.store.pool)
        .await
        .unwrap();
    assert_eq!(reservations, 1);
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn zero_input_ceiling_only_for_v3_without_input_token_meters(pool: PgPool) {
    let f = fixture(pool).await;
    // Speech-like price: characters only, every token meter not applicable.
    let speech: Vec<Value> = Meter::ALL
        .into_iter()
        .map(|m| match m {
            Meter::InputCharacters => line(m, "15000000", 1_000_000),
            Meter::Requests => line(m, "0", 1),
            m => na(m),
        })
        .collect();
    let insert = |lines: Vec<Value>, version: i16| {
        let pool = f.store.pool.clone();
        let deployment = f.deployment;
        async move {
            if version == 3 {
                sqlx::query("INSERT INTO deployment_prices(id,deployment_id,input_token_limit,output_token_limit,pricing_version,price_lines,max_units) VALUES($1,$2,0,0,3,$3,'{\"input_characters\":\"200\"}')").bind(Uuid::new_v4()).bind(deployment).bind(json!(lines)).execute(&pool).await
            } else {
                sqlx::query("INSERT INTO deployment_prices(id,deployment_id,input_microusd_per_million,output_microusd_per_million,input_token_limit,output_token_limit,pricing_version) VALUES($1,$2,1,1,0,0,1)").bind(Uuid::new_v4()).bind(deployment).execute(&pool).await
            }
        }
    };
    assert!(
        insert(vec![], 1).await.is_err(),
        "v1/v2 keep a positive ceiling"
    );
    insert(speech.clone(), 3).await.unwrap();
    sqlx::query("UPDATE models SET supported_protocols=ARRAY['audio_speech'] WHERE id=$1")
        .bind(f.model)
        .execute(&f.store.pool)
        .await
        .unwrap();
    let deployment = f
        .store
        .deployments(&f.principal, "company/smart")
        .await
        .unwrap()
        .remove(0);
    let admission = crate::inference::workload::WorkloadAdmission {
        kind: WorkloadKind::AudioSpeech,
        output: crate::inference::workload::OutputReservation::None,
        unit_ceilings: MeterUsage {
            input_characters: Some(11),
            ..Default::default()
        },
    };
    f.policy(
        "workspace_local_policies",
        None,
        Some(1),
        None,
        Some(1_000_000),
    )
    .await;
    let start = f.start();
    admit_workload_for_deployment(&f.store, &start, &admission, 30, &deployment)
        .await
        .unwrap();
    // ceil(11 × 15) characters; zero token reservation fits any token limit.
    assert_eq!(amounts(&f, start.id).await.1, Some(165));
    // A priced (or unknown) input token meter cannot use a zero ceiling.
    let mut tokens = speech.clone();
    tokens.retain(|l| l["meter"] != "input_tokens");
    tokens.push(line(Meter::InputTokens, "1", 1_000_000));
    insert(tokens, 3).await.unwrap();
    assert_eq!(
        admit_workload_for_deployment(&f.store, &f.start(), &admission, 30, &deployment).await,
        Err(InferenceError::Configuration)
    );
}

/// `price_meters` (the publication completeness rule) is exactly the set of
/// meters each workload's admission bound needs: stating only those bounds
/// the hold, and leaving any one of them out makes it unbounded.
#[test]
fn price_meters_are_exactly_what_each_workload_admission_bounds() {
    use crate::inference::{
        audio::transcription_meters, realtime::ResponseBound, types::WorkloadKind as K,
    };
    let price = |meters: &[Meter]| {
        let lines: Vec<Value> = meters
            .iter()
            .map(|m| line(*m, "1000", m.batches()[0]))
            .collect();
        let max: serde_json::Map<String, Value> = meters
            .iter()
            .filter(|m| !m.is_token() && !m.is_audio_token())
            .map(|m| (m.as_str().to_owned(), json!("1000000")))
            .collect();
        Price {
            id: Uuid::nil(),
            input_microusd_per_million: None,
            output_microusd_per_million: None,
            input_token_limit: 100,
            output_token_limit: 50,
            pricing_version: 3,
            cache_pricing: None,
            price_lines: Some(json!(lines)),
            max_units: Some(Value::Object(max)),
            batch_price_lines: None,
        }
    };
    let images = MeterUsage {
        output_images: Some(1),
        input_characters: Some(0),
        input_audio_seconds_ms: Some(0),
        output_audio_seconds_ms: Some(0),
        search_units: Some(0),
        requests: Some(1),
        output_video_seconds_ms: None,
    };
    let speech = MeterUsage {
        output_images: Some(0),
        input_characters: Some(12),
        input_audio_seconds_ms: Some(0),
        output_audio_seconds_ms: None,
        search_units: Some(0),
        requests: Some(1),
        output_video_seconds_ms: None,
    };
    let once = MeterUsage {
        requests: Some(1),
        ..MeterUsage::default()
    };
    for kind in [
        K::Generation,
        K::Embeddings,
        K::Images,
        K::AudioTranscriptions,
        K::AudioSpeech,
        K::Rerank,
        K::Systemone,
        K::Realtime,
        K::Videos,
        K::Batches,
    ] {
        let hold = |p: &Price| -> Option<i64> {
            match kind {
                // Any later window (unknown context) can hold audio input.
                K::Realtime => p.realtime_window(ResponseBound::unknown(10)).unwrap(),
                K::Videos => p
                    .bound_scaled(1, 10, &crate::jobs::unit_ceilings(1, 4000), true)
                    .unwrap(),
                K::Images => p.bound(10, &images).unwrap(),
                K::AudioTranscriptions => p.bound(10, &transcription_meters(Some(2000))).unwrap(),
                K::AudioSpeech => p.bound(10, &speech).unwrap(),
                K::Rerank | K::Systemone => p.bound(10, &once).unwrap(),
                K::Generation | K::Embeddings | K::Batches => {
                    p.bound(10, &MeterUsage::default()).unwrap()
                }
            }
        };
        let meters = price_meters(kind);
        assert!(hold(&price(&meters)).is_some(), "{kind:?}");
        for left_out in &meters {
            let rest: Vec<Meter> = meters.iter().copied().filter(|m| m != left_out).collect();
            if rest.iter().all(|m| m.is_audio_token()) {
                continue;
            }
            assert!(
                hold(&price(&rest)).is_none(),
                "{kind:?} without {left_out:?}"
            );
        }
    }
}
