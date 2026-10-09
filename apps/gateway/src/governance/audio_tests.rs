//! Speech workload admission and settlement on a disposable database: v3
//! per-minute transcription and per-M-character speech prices, measured
//! request ceilings vs. price `max_units`, budgets and cancellation.
use super::tests::db::{Fixture, amounts, fixture};
use super::*;
use crate::{
    billing::v3::Meter,
    inference::{
        audio::{
            AudioFormat, SpeechFormat, SpeechRequest, TranscriptionFormat, TranscriptionRequest,
            tests_support::wav, transcription_meters,
        },
        error::LimitScope,
        repository::InferenceRepository,
        workload::Workload,
    },
    providers::metering,
};
use serde_json::{Value, json};
use sqlx::PgPool;
use std::sync::Arc;

fn line(meter: Meter, amount: &str, batch: u64) -> Value {
    json!({"meter":meter.as_str(),"microusd_per_batch":amount,"batch":batch,"unit_label":meter.unit_label(batch).unwrap(),"sku_label":"Line"})
}
fn na(meter: Meter) -> Value {
    json!({"meter":meter.as_str(),"not_applicable":true})
}
/// All meters not applicable except `priced` lines and a free request line.
fn lines(priced: Vec<Value>) -> Vec<Value> {
    let mut out: Vec<Value> = Meter::ALL
        .into_iter()
        .filter(|m| !priced.iter().any(|l| l["meter"] == m.as_str()) && *m != Meter::Requests)
        .map(na)
        .collect();
    out.extend(priced);
    out.push(line(Meter::Requests, "0", 1));
    out
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
fn finished(id: Uuid, outcome: Outcome, usage: Usage) -> ExecutionFinish {
    ExecutionFinish {
        id,
        outcome,
        error: None,
        usage,
        elapsed_ms: 5,
    }
}
fn transcription(audio: Vec<u8>, format: AudioFormat) -> TranscriptionRequest {
    TranscriptionRequest::new(
        "company/smart".into(),
        Arc::from(audio),
        format,
        None,
        None,
        None,
        TranscriptionFormat::Json,
    )
}
fn speech(input: &str) -> SpeechRequest {
    SpeechRequest {
        model: "company/smart".into(),
        input: input.into(),
        voice: "alloy".into(),
        response_format: SpeechFormat::Mp3,
        speed: None,
    }
}
fn audio_usage(ms: u64) -> Usage {
    Usage {
        meters: Some(transcription_meters(Some(ms))),
        ..Usage::default()
    }
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn per_minute_transcription_holds_measured_duration_and_settles_reported(pool: PgPool) {
    let f = fixture(pool).await;
    let deployment = protocols(&f, "audio_transcriptions").await;
    // $0.006/minute.
    price(
        &f,
        lines(vec![line(Meter::InputAudioSecondsMs, "6000", 60_000)]),
        json!({}),
    )
    .await;
    // 0.85 s measured → ceiling 2 s → hold ceil(2000 × 6000 / 60000) = 200.
    budget(&f, Some(200)).await;
    let request = transcription(wav(850), AudioFormat::Wav);
    let start = f.start();
    admit_workload_for_deployment(&f.store, &start, &request.admission(), 30, &deployment)
        .await
        .unwrap();
    assert_eq!(
        amounts(&f, start.id).await,
        ("pending".into(), Some(200), None, false)
    );
    // whisper-1 reports one whole second: 100 µUSD.
    finish(
        &f.store,
        &finished(start.id, Outcome::Succeeded, audio_usage(1000)),
    )
    .await
    .unwrap();
    assert_eq!(
        amounts(&f, start.id).await,
        ("settled".into(), Some(200), Some(100), false)
    );
    let row: (String, Value, Value) = sqlx::query_as("SELECT e.workload_kind,r.meter_usage,r.cost_components FROM inference_executions e JOIN governance_reservations r ON r.execution_id=e.id WHERE e.id=$1").bind(start.id).fetch_one(&f.store.pool).await.unwrap();
    assert_eq!(row.0, "audio_transcriptions");
    assert_eq!(row.1["input_audio_seconds_ms"], "1000");
    assert_eq!(row.2["input_audio_microusd"], "100");
    // 100 µUSD remain: another 200 hold is denied.
    assert_eq!(
        admit_workload_for_deployment(&f.store, &f.start(), &request.admission(), 30, &deployment)
            .await,
        Err(InferenceError::BudgetExceeded(LimitScope::Workspace))
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn unmeasured_audio_needs_price_max_units_and_ceilings_never_loosen_it(pool: PgPool) {
    let f = fixture(pool).await;
    let deployment = protocols(&f, "audio_transcriptions").await;
    let per_minute = || lines(vec![line(Meter::InputAudioSecondsMs, "6000", 60_000)]);
    price(&f, per_minute(), json!({})).await;
    budget(&f, Some(1_000_000)).await;
    // M4A duration is not measured: without max_units the hold is unbounded.
    let m4a = transcription(b"\0\0\0\x18ftypM4A ".to_vec(), AudioFormat::Mp4);
    assert_eq!(m4a.admission().unit_ceilings.input_audio_seconds_ms, None);
    assert_eq!(
        admit_workload_for_deployment(&f.store, &f.start(), &m4a.admission(), 30, &deployment)
            .await,
        Err(InferenceError::PriceUnbounded)
    );
    // A ten-minute max_units bounds it at $0.06.
    price(&f, per_minute(), json!({"input_audio_seconds_ms":"600000"})).await;
    let start = f.start();
    admit_workload_for_deployment(&f.store, &start, &m4a.admission(), 30, &deployment)
        .await
        .unwrap();
    assert_eq!(amounts(&f, start.id).await.1, Some(60_000));
    // A measured request tightens the hold to its own ceiling.
    let short = transcription(wav(850), AudioFormat::Wav);
    let start = f.start();
    admit_workload_for_deployment(&f.store, &start, &short.admission(), 30, &deployment)
        .await
        .unwrap();
    assert_eq!(amounts(&f, start.id).await.1, Some(200));
    // Measured audio longer than the priced maximum is not covered: unbounded.
    price(&f, per_minute(), json!({"input_audio_seconds_ms":"1000"})).await;
    assert_eq!(
        admit_workload_for_deployment(&f.store, &f.start(), &short.admission(), 30, &deployment)
            .await,
        Err(InferenceError::PriceUnbounded)
    );
    budget(&f, None).await;
    let start = f.start();
    admit_workload_for_deployment(&f.store, &start, &short.admission(), 30, &deployment)
        .await
        .unwrap();
    assert_eq!(
        amounts(&f, start.id).await,
        ("pending".into(), None, None, true)
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn token_billed_transcription_settles_tokens_with_free_audio_line(pool: PgPool) {
    let f = fixture(pool).await;
    let deployment = protocols(&f, "audio_transcriptions").await;
    // gpt-4o-mini-transcribe style: $1.25/M input, $5/M output; the measured
    // duration is observed but explicitly free.
    price(
        &f,
        lines(vec![
            line(Meter::InputTokens, "1250000", 1_000_000),
            line(Meter::OutputTokens, "5000000", 1_000_000),
            line(Meter::InputAudioSecondsMs, "0", 60_000),
        ]),
        json!({}),
    )
    .await;
    let start = f.start();
    let request = transcription(wav(850), AudioFormat::Wav);
    admit_workload_for_deployment(&f.store, &start, &request.admission(), 30, &deployment)
        .await
        .unwrap();
    // 100 input × 1.25 + 50 output (price ceiling) × 5.
    assert_eq!(amounts(&f, start.id).await.1, Some(125 + 250));
    let mut usage = metering::input_only(Some(8));
    usage.output_tokens = Some(6);
    usage.meters = Some(transcription_meters(Some(850)));
    finish(&f.store, &finished(start.id, Outcome::Succeeded, usage))
        .await
        .unwrap();
    assert_eq!(amounts(&f, start.id).await.2, Some(10 + 30));
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn per_million_character_speech_holds_exact_characters(pool: PgPool) {
    let f = fixture(pool).await;
    let deployment = protocols(&f, "audio_speech").await;
    // $15/M characters (MAI-Voice-2-Flash); output audio not applicable.
    price(
        &f,
        lines(vec![line(Meter::InputCharacters, "15000000", 1_000_000)]),
        json!({}),
    )
    .await;
    let request = speech("Hello world!");
    assert_eq!(request.input_characters(), 12);
    // ceil(12 × 15) = 180 µUSD exactly fits the budget.
    budget(&f, Some(180)).await;
    let mut start = f.start();
    start.streamed = true;
    admit_workload_for_deployment(&f.store, &start, &request.admission(), 30, &deployment)
        .await
        .unwrap();
    assert_eq!(amounts(&f, start.id).await.1, Some(180));
    finish(
        &f.store,
        &finished(start.id, Outcome::Succeeded, request.usage()),
    )
    .await
    .unwrap();
    assert_eq!(
        amounts(&f, start.id).await,
        ("settled".into(), Some(180), Some(180), false)
    );
    let row: (String, bool, Value) = sqlx::query_as("SELECT e.workload_kind,e.streamed,r.meter_usage FROM inference_executions e JOIN governance_reservations r ON r.execution_id=e.id WHERE e.id=$1").bind(start.id).fetch_one(&f.store.pool).await.unwrap();
    assert_eq!((row.0.as_str(), row.1), ("audio_speech", true));
    assert_eq!(row.2["input_characters"], "12");
    assert_eq!(row.2["output_audio_seconds_ms"], Value::Null);
    assert_eq!(
        admit_workload_for_deployment(
            &f.store,
            &f.start(),
            &speech("a").admission(),
            30,
            &deployment
        )
        .await,
        Err(InferenceError::BudgetExceeded(LimitScope::Workspace))
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn cancelled_speech_keeps_its_hold_and_unbounded_output_is_refused(pool: PgPool) {
    let f = fixture(pool).await;
    let deployment = protocols(&f, "audio_speech").await;
    price(
        &f,
        lines(vec![line(Meter::InputCharacters, "15000000", 1_000_000)]),
        json!({}),
    )
    .await;
    let request = speech("Hi there");
    let start = f.start();
    admit_workload_for_deployment(&f.store, &start, &request.admission(), 30, &deployment)
        .await
        .unwrap();
    // A client disconnect mid-body: the provider may still charge.
    finish(
        &f.store,
        &finished(start.id, Outcome::Cancelled, request.usage()),
    )
    .await
    .unwrap();
    assert_eq!(
        amounts(&f, start.id).await,
        ("unknown".into(), Some(120), None, false)
    );
    // Character input beyond the priced max_units is not covered.
    price(
        &f,
        lines(vec![line(Meter::InputCharacters, "15000000", 1_000_000)]),
        json!({"input_characters":"4"}),
    )
    .await;
    budget(&f, Some(1_000_000)).await;
    assert_eq!(
        admit_workload_for_deployment(&f.store, &f.start(), &request.admission(), 30, &deployment)
            .await,
        Err(InferenceError::PriceUnbounded)
    );
    // Priced output audio (per minute) cannot be bounded by the request.
    price(
        &f,
        lines(vec![
            line(Meter::InputCharacters, "0", 1_000_000),
            line(Meter::OutputAudioSecondsMs, "150000", 60_000),
        ]),
        json!({}),
    )
    .await;
    assert_eq!(
        admit_workload_for_deployment(&f.store, &f.start(), &request.admission(), 30, &deployment)
            .await,
        Err(InferenceError::PriceUnbounded)
    );
    price(
        &f,
        lines(vec![
            line(Meter::InputCharacters, "0", 1_000_000),
            line(Meter::OutputAudioSecondsMs, "150000", 60_000),
        ]),
        json!({"output_audio_seconds_ms":"60000"}),
    )
    .await;
    let start = f.start();
    admit_workload_for_deployment(&f.store, &start, &request.admission(), 30, &deployment)
        .await
        .unwrap();
    assert_eq!(amounts(&f, start.id).await.1, Some(150_000));
    // Output duration is never reported: the settlement stays unknown.
    finish(
        &f.store,
        &finished(start.id, Outcome::Succeeded, request.usage()),
    )
    .await
    .unwrap();
    assert_eq!(amounts(&f, start.id).await.0, "unknown");
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn unreported_tokens_are_zero_only_when_the_price_says_not_applicable(pool: PgPool) {
    let f = fixture(pool).await;
    let deployment = protocols(&f, "audio_transcriptions").await;
    // Free (not NA) token lines: an unreported count is still unknown, so the
    // settlement stays unknown instead of inventing zero tokens.
    price(
        &f,
        lines(vec![
            line(Meter::InputTokens, "0", 1_000_000),
            line(Meter::OutputTokens, "0", 1_000_000),
            line(Meter::InputAudioSecondsMs, "6000", 60_000),
        ]),
        json!({}),
    )
    .await;
    let start = f.start();
    let request = transcription(wav(850), AudioFormat::Wav);
    admit_workload_for_deployment(&f.store, &start, &request.admission(), 30, &deployment)
        .await
        .unwrap();
    finish(
        &f.store,
        &finished(start.id, Outcome::Succeeded, audio_usage(1000)),
    )
    .await
    .unwrap();
    assert_eq!(
        amounts(&f, start.id).await,
        ("unknown".into(), Some(200), None, false)
    );
    let tokens: (Option<i64>, Option<i64>) =
        sqlx::query_as("SELECT input_tokens,output_tokens FROM inference_executions WHERE id=$1")
            .bind(start.id)
            .fetch_one(&f.store.pool)
            .await
            .unwrap();
    assert_eq!(tokens, (None, None));
    // Identical finishes stay idempotent.
    finish(
        &f.store,
        &finished(start.id, Outcome::Succeeded, audio_usage(1000)),
    )
    .await
    .unwrap();
}
