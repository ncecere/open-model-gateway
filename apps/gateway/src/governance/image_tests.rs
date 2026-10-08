//! Image generation admission and settlement on a disposable database: v3
//! per-image variant tiers (OpenRouter FLUX style) and token pricing
//! (OpenAI gpt-image style), request-derived `n` ceilings, budgets, and
//! unpriced variants/meters.
use super::tests::db::{Fixture, amounts, fixture};
use super::*;
use crate::{
    billing::v3::Meter,
    inference::{
        error::LimitScope,
        images::{ImageRequest, ImageSize, ImageTier},
        repository::InferenceRepository,
        workload::Workload,
    },
    providers::images::meters,
};
use serde_json::{Value, json};
use sqlx::PgPool;

fn line(meter: Meter, amount: &str, batch: u64) -> Value {
    json!({"meter":meter.as_str(),"microusd_per_batch":amount,"batch":batch,"unit_label":meter.unit_label(batch).unwrap(),"sku_label":"Line"})
}
fn image(amount: &str, variant: &str) -> Value {
    let mut l = line(Meter::OutputImages, amount, 1);
    l["variant"] = json!(variant);
    l
}
fn na(meter: Meter) -> Value {
    json!({"meter":meter.as_str(),"not_applicable":true})
}
/// Character/audio/search meters are deliberately left unpriced: image
/// admission ceilings for them are zero, so they never make a hold unbounded.
fn flux_lines(images: Vec<Value>) -> Vec<Value> {
    let mut lines = vec![
        line(Meter::InputTokens, "0", 1_000_000),
        line(Meter::OutputTokens, "0", 1_000_000),
        na(Meter::CacheReadTokens),
        na(Meter::CacheWriteTokens),
        na(Meter::CacheWrite5mTokens),
        na(Meter::CacheWrite1hTokens),
        line(Meter::Requests, "0", 1),
    ];
    lines.extend(images);
    lines
}
async fn price(f: &Fixture, lines: Vec<Value>, input: i64, output: i64) {
    sqlx::query("INSERT INTO deployment_prices(id,deployment_id,input_token_limit,output_token_limit,pricing_version,price_lines,max_units) VALUES($1,$2,$3,$4,3,$5,'{}')").bind(Uuid::new_v4()).bind(f.deployment).bind(input).bind(output).bind(json!(lines)).execute(&f.store.pool).await.unwrap();
}
async fn images_model(f: &Fixture) -> Deployment {
    sqlx::query("UPDATE models SET supported_protocols=ARRAY['images'] WHERE id=$1")
        .bind(f.model)
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
fn request(n: u32) -> ImageRequest {
    ImageRequest {
        model: "company/smart".into(),
        prompt: "p".into(),
        n,
        size: Some(ImageSize::Tier(ImageTier::P768)),
        quality: None,
        seed: None,
        max_response_bytes: 1 << 20,
    }
}
fn usage(input: u64, output: u64, images: u64, variant: &str) -> Usage {
    Usage {
        input_tokens: Some(input),
        output_tokens: Some(output),
        billing: Some(BillingUsage {
            total_input_tokens: Some(input),
            uncached_input_tokens: Some(input),
            cache_read_input_tokens: Some(0),
            cache_write_input_tokens: Some(0),
            cache_write_default_input_tokens: Some(0),
            cache_write_5m_input_tokens: Some(0),
            cache_write_1h_input_tokens: Some(0),
        }),
        meters: Some(meters(Some(images))),
        output_image_variant: MeterVariant::new(variant),
        provider_cost_microusd: Some(20_500),
        reasoning_tokens: None,
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

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn per_image_tiers_hold_n_times_highest_variant_and_settle_the_billed_tier(pool: PgPool) {
    let f = fixture(pool).await;
    let deployment = images_model(&f).await;
    price(
        &f,
        flux_lines(vec![
            image("20500", "768"),
            image("24000", "1k"),
            image("303500", "4k"),
        ]),
        100,
        10_000,
    )
    .await;
    // n=2 × the highest tier (4K): exactly the budget.
    budget(&f, Some(607_000)).await;
    let start = f.start();
    admit_workload_for_deployment(&f.store, &start, &request(2).admission(), 30, &deployment)
        .await
        .unwrap();
    assert_eq!(
        amounts(&f, start.id).await,
        ("pending".into(), Some(607_000), None, false)
    );
    // Two 768 images; synthetic image tokens are free; cost is evidence only.
    finish(&f.store, &finished(start.id, usage(0, 8350, 2, "768")))
        .await
        .unwrap();
    assert_eq!(
        amounts(&f, start.id).await,
        ("settled".into(), Some(607_000), Some(41_000), false)
    );
    let row: (Value, Value, Option<String>, Option<i64>, String) = sqlx::query_as("SELECT r.cost_components,r.meter_usage,r.output_image_variant,r.provider_cost_microusd,e.workload_kind FROM governance_reservations r JOIN inference_executions e ON e.id=r.execution_id WHERE r.execution_id=$1").bind(start.id).fetch_one(&f.store.pool).await.unwrap();
    assert_eq!(row.0["output_images_microusd"], "41000");
    assert_eq!(row.1["output_images"], "2");
    assert_eq!(row.1["requests"], "1");
    assert_eq!(row.1["input_characters"], "0");
    assert_eq!(row.2.as_deref(), Some("768"));
    assert_eq!(row.3, Some(20_500));
    assert_eq!(row.4, "images");
    // 566,000 remain: one more image (303,500) fits, two do not.
    assert_eq!(
        admit_workload_for_deployment(
            &f.store,
            &f.start(),
            &request(2).admission(),
            30,
            &deployment
        )
        .await,
        Err(InferenceError::BudgetExceeded(LimitScope::Workspace))
    );
    admit_workload_for_deployment(
        &f.store,
        &f.start(),
        &request(1).admission(),
        30,
        &deployment,
    )
    .await
    .unwrap();
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn token_priced_gpt_image_holds_token_ceilings_and_settles_tokens(pool: PgPool) {
    let f = fixture(pool).await;
    let deployment = images_model(&f).await;
    // gpt-image-1-mini style: $2/M text input, $0.20/M cached, $8/M image
    // output; the image count itself is free.
    price(
        &f,
        vec![
            line(Meter::InputTokens, "2000000", 1_000_000),
            line(Meter::OutputTokens, "8000000", 1_000_000),
            line(Meter::CacheReadTokens, "200000", 1_000_000),
            na(Meter::CacheWriteTokens),
            na(Meter::CacheWrite5mTokens),
            na(Meter::CacheWrite1hTokens),
            line(Meter::OutputImages, "0", 1),
            line(Meter::Requests, "0", 1),
        ],
        1000,
        4000,
    )
    .await;
    budget(&f, Some(1_000_000)).await;
    let start = f.start();
    let mut r = request(1);
    r.size = None;
    admit_workload_for_deployment(&f.store, &start, &r.admission(), 30, &deployment)
        .await
        .unwrap();
    // 1000 input × $2/M + 1000 cached × $0.20/M + 4000 output × $8/M.
    assert_eq!(amounts(&f, start.id).await.1, Some(2000 + 200 + 32_000));
    let mut observed = usage(9, 272, 1, "1024x1024");
    observed.provider_cost_microusd = None;
    finish(&f.store, &finished(start.id, observed))
        .await
        .unwrap();
    // ceil(9 × 2) + ceil(272 × 8) = 18 + 2176.
    assert_eq!(
        amounts(&f, start.id).await,
        ("settled".into(), Some(34_200), Some(2194), false)
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn unpriced_images_are_unbounded_and_unpriced_variants_stay_unknown(pool: PgPool) {
    let f = fixture(pool).await;
    let deployment = images_model(&f).await;
    // No output_images line at all: the image meter is unknown, not free.
    price(&f, flux_lines(vec![]), 100, 10_000).await;
    budget(&f, Some(10_000_000)).await;
    assert_eq!(
        admit_workload_for_deployment(
            &f.store,
            &f.start(),
            &request(1).admission(),
            30,
            &deployment
        )
        .await,
        Err(InferenceError::BudgetExceeded(LimitScope::Workspace))
    );
    // Variant tiers without a matching line for the billed tier: the hold
    // is finite (closed set) but the settlement is unresolved.
    price(
        &f,
        flux_lines(vec![image("20500", "768"), image("303500", "4k")]),
        100,
        10_000,
    )
    .await;
    budget(&f, None).await;
    let start = f.start();
    admit_workload_for_deployment(&f.store, &start, &request(1).admission(), 30, &deployment)
        .await
        .unwrap();
    assert_eq!(amounts(&f, start.id).await.1, Some(303_500));
    finish(&f.store, &finished(start.id, usage(0, 4175, 1, "2k")))
        .await
        .unwrap();
    let (state, _, actual, unbounded) = amounts(&f, start.id).await;
    assert_eq!((state.as_str(), actual, unbounded), ("unknown", None, true));
}
