//! Rows for the slice 6 contract fixtures, written exactly as their producers
//! write them (admission, settlement, realtime close, retention compaction).
//! Only valid state: every CHECK, trigger and foreign key of the schema applies.
use super::*;
use chrono::{DateTime, Duration, Utc};

/// All seven keys present (a complete provider usage report).
pub(super) const BILLING_FULL: &str = r#"{"total_input_tokens":"10","uncached_input_tokens":"7","cache_read_input_tokens":"3","cache_write_input_tokens":"0","cache_write_default_input_tokens":"0","cache_write_5m_input_tokens":"0","cache_write_1h_input_tokens":"0"}"#;
/// All seven keys present, every figure unknown (the provider reported none).
pub(super) const BILLING_UNKNOWN: &str = r#"{"total_input_tokens":null,"uncached_input_tokens":null,"cache_read_input_tokens":null,"cache_write_input_tokens":null,"cache_write_default_input_tokens":null,"cache_write_5m_input_tokens":null,"cache_write_1h_input_tokens":null}"#;
pub(super) const METER_FULL: &str = r#"{"output_images":"1","input_characters":"2","input_audio_seconds_ms":"3","output_audio_seconds_ms":"4","search_units":"5","requests":"6","output_video_seconds_ms":"7"}"#;
pub(super) const METER_UNKNOWN: &str = r#"{"output_images":null,"input_characters":null,"input_audio_seconds_ms":null,"output_audio_seconds_ms":null,"search_units":null,"requests":null}"#;

/// What the accounting side recorded for an attempt.
#[derive(Clone, Copy)]
pub(super) enum Res {
    /// No reservation row at all (an interrupted attempt).
    Missing,
    Settled {
        actual: i64,
        price: Option<Uuid>,
    },
    Pending {
        held: i64,
        price: Option<Uuid>,
    },
    Unknown {
        held: Option<i64>,
        price: Option<Uuid>,
        unbounded: bool,
    },
}

#[derive(Clone, Copy)]
pub(super) struct Ex<'a> {
    pub ws: Uuid,
    pub key: Uuid,
    pub deployment: Uuid,
    pub root: Uuid,
    pub n: i32,
    pub model: &'a str,
    pub provider: &'a str,
    /// Minutes before now that the ROOT started; attempt n starts n seconds later.
    pub ago: i64,
    pub state: &'a str,
    pub error: Option<&'a str>,
    pub streamed: bool,
    pub workload: &'a str,
    pub tokens: Option<(i64, i64)>,
    pub reasoning: Option<i64>,
    pub finish: Option<&'a str>,
    pub ttft: Option<i64>,
    pub generation: Option<i64>,
    pub elapsed: Option<i64>,
    pub session: Option<&'a str>,
    pub app: Option<&'a str>,
    pub upstream: Option<&'a str>,
    pub reported: Option<&'a str>,
    pub billing: Option<&'a str>,
    pub meter: Option<&'a str>,
    pub completed: bool,
    pub cost_center: Option<Uuid>,
    pub res: Res,
    pub redacted: bool,
}
impl Ex<'_> {
    /// A successful, fully described attempt; override what a case differs in.
    pub(super) fn rich(ws: Uuid, key: Uuid, deployment: Uuid, root: Uuid, ago: i64) -> Self {
        Ex {
            ws,
            key,
            deployment,
            root,
            n: 1,
            model: "contract-log",
            provider: "openai",
            ago,
            state: "succeeded",
            error: None,
            streamed: true,
            workload: "generation",
            tokens: Some((10, 5)),
            reasoning: Some(2),
            finish: Some("stop"),
            ttft: Some(100),
            generation: Some(600),
            elapsed: Some(250),
            session: Some("s-rich"),
            app: Some("Agent"),
            upstream: Some("snap-upstream"),
            reported: Some("served-upstream-v2"),
            billing: Some(BILLING_FULL),
            meter: Some(METER_FULL),
            completed: true,
            cost_center: None,
            res: Res::Settled {
                actual: 7,
                price: None,
            },
            redacted: false,
        }
    }
    /// An attempt that recorded nothing optional.
    pub(super) fn sparse(ws: Uuid, key: Uuid, deployment: Uuid, root: Uuid, ago: i64) -> Self {
        Ex {
            tokens: None,
            reasoning: None,
            finish: None,
            ttft: None,
            generation: None,
            elapsed: None,
            session: None,
            app: None,
            upstream: None,
            reported: None,
            billing: None,
            meter: None,
            streamed: false,
            res: Res::Missing,
            ..Ex::rich(ws, key, deployment, root, ago)
        }
    }
}

/// The attempt exactly as the engine, settlement and compaction leave it.
pub(super) async fn exec(pool: &PgPool, e: Ex<'_>) -> Uuid {
    let id = if e.n == 1 { e.root } else { Uuid::new_v4() };
    let started: DateTime<Utc> =
        Utc::now() - Duration::minutes(e.ago) + Duration::seconds(e.n.into());
    let completed = e.completed.then(|| started + Duration::milliseconds(250));
    // Retention compaction clears these four and records when (maintenance.rs).
    let (error, elapsed, session, app) = if e.redacted {
        (None, None, None, None)
    } else {
        (e.error, e.elapsed, e.session, e.app)
    };
    let (input, output) = e.tokens.unzip();
    sqlx::query("INSERT INTO inference_executions(id,workspace_id,api_key_id,deployment_id,public_model,provider,streamed,state,error_code,input_tokens,output_tokens,billing_usage,workload_kind,elapsed_ms,started_at,completed_at,root_request_id,attempt_number,details_redacted_at,cost_center_id,cost_center_name,cost_center_code,finish_reason,time_to_first_token_ms,generation_ms,reasoning_tokens,upstream_model,client_session_id,client_app,reported_upstream_model,meter_usage) SELECT $1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12::text::jsonb,$13,$14,$15,$16,$17,$18,CASE WHEN $19 THEN clock_timestamp() END,c.id,c.name,c.code,$21,$22,$23,$24,$25,$26,$27,$28,$29::text::jsonb FROM (SELECT 1) one LEFT JOIN cost_centers c ON c.id=$20")
        .bind(id).bind(e.ws).bind(e.key).bind(e.deployment).bind(e.model).bind(e.provider).bind(e.streamed).bind(e.state).bind(error)
        .bind(input).bind(output).bind(e.billing).bind(e.workload).bind(elapsed).bind(started).bind(completed).bind(e.root).bind(e.n)
        .bind(e.redacted).bind(e.cost_center).bind(e.finish).bind(e.ttft).bind(e.generation).bind(e.reasoning).bind(e.upstream).bind(session).bind(app).bind(e.reported).bind(e.meter)
        .execute(pool).await.unwrap();
    let (state, actual, held, price, unbounded) = match e.res {
        Res::Missing => return id,
        Res::Settled { actual, price } => ("settled", Some(actual), None, price, false),
        Res::Pending { held, price } => ("pending", None, Some(held), price, false),
        Res::Unknown {
            held,
            price,
            unbounded,
        } => ("unknown", None, held, price, unbounded),
    };
    // A settled reservation carries the usage it settled on.
    let settled = e.tokens.unwrap_or((0, 0));
    sqlx::query("INSERT INTO governance_reservations(execution_id,workspace_id,api_key_id,deployment_id,price_id,admitted_at,minute_start,month_start,lease_expires_at,state,held_microusd,actual_microusd,input_tokens,output_tokens,unbounded_cost) SELECT e.id,e.workspace_id,e.api_key_id,e.deployment_id,$2,e.started_at,date_trunc('minute',e.started_at,'UTC'),date_trunc('month',e.started_at,'UTC'),e.started_at,$3,$4,$5,CASE WHEN $3='settled' THEN $6::bigint END,CASE WHEN $3='settled' THEN $7::bigint END,$8 FROM inference_executions e WHERE e.id=$1")
        .bind(id).bind(price).bind(state).bind(held).bind(actual).bind(settled.0).bind(settled.1).bind(unbounded)
        .execute(pool).await.unwrap();
    id
}

/// A priced deployment version (pricing v1: flat per-million token rates).
pub(super) async fn price(pool: &PgPool, deployment: Uuid) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO deployment_prices(id,deployment_id,input_microusd_per_million,output_microusd_per_million,input_token_limit,output_token_limit,pricing_version) VALUES($1,$2,1000000,1000000,1000,1000,1)")
        .bind(id).bind(deployment).execute(pool).await.unwrap();
    id
}

/// One response row of a realtime session, as `governance::realtime` writes it:
/// `pending` while the response runs; `settled` with usage; `unknown` (and
/// completed) when the session closed before the response did.
pub(super) async fn realtime_response(pool: &PgPool, execution: Uuid, sequence: i32, state: &str) {
    let sql = match state {
        "settled" => "INSERT INTO realtime_responses(execution_id,sequence,state,status,window_hold_microusd,actual_microusd,unbounded_cost,input_text_tokens,cached_text_tokens,input_audio_tokens,cached_audio_tokens,output_text_tokens,output_audio_tokens,completed_at) VALUES($1,$2,'settled','completed',6,4,false,3,1,2,0,5,6,clock_timestamp()+interval '40 milliseconds')",
        "unknown" => "INSERT INTO realtime_responses(execution_id,sequence,state,window_hold_microusd,unbounded_cost,completed_at) VALUES($1,$2,'unknown',6,true,clock_timestamp()+interval '40 milliseconds')",
        _ => "INSERT INTO realtime_responses(execution_id,sequence,state,window_hold_microusd,unbounded_cost) VALUES($1,$2,'pending',6,false)",
    };
    sqlx::query(sql)
        .bind(execution)
        .bind(sequence)
        .execute(pool)
        .await
        .unwrap();
}
