//! Accepted provider jobs preserve missing usage on free and priced paths.
use super::*;
use crate::{
    billing::{MeterUsage, v3::Meter},
    governance::tests::db::amounts,
    inference::{
        types::WorkloadKind,
        workload::{OutputReservation, WorkloadAdmission},
    },
};
use gateway_datetime_compat::UtcInstant;

type StoredUsage = (
    Option<i64>,
    Option<i64>,
    Option<Value>,
    Option<Value>,
    Option<i64>,
);

async fn price(f: &Fixture, free: bool) {
    let lines: Vec<Value> = Meter::ALL.into_iter().chain(Meter::VIDEO).map(|m| {
        let amount = if free || !matches!(m, Meter::InputTokens | Meter::OutputTokens) { "0" } else { "1000000" };
        json!({"meter":m.as_str(),"microusd_per_batch":amount,"batch":m.batches()[0],"unit_label":m.unit_label(m.batches()[0]).unwrap(),"sku_label":"Test"})
    }).collect();
    sqlx::query("INSERT INTO deployment_prices(id,deployment_id,input_token_limit,output_token_limit,pricing_version,price_lines,batch_price_lines,max_units) VALUES($1,$2,100,50,3,$3,$3,'{}')")
        .bind(Uuid::new_v4()).bind(f.deployment).bind(json!(lines)).execute(&f.store.pool).await.unwrap();
}

async fn unknown(f: &Fixture, id: Uuid, observed: Usage, held: Option<i64>) {
    let a = amounts(f, id).await;
    assert_eq!(a.0, "unknown");
    assert_eq!(a.1, held);
    assert_eq!(a.2, None);
    let record = ExecutionFinish {
        id,
        outcome: Outcome::Failed,
        error: Some(InferenceError::UpstreamRejected),
        usage: observed,
        elapsed_ms: 0,
    };
    crate::governance::finish_accepted_job(&f.store, &record)
        .await
        .unwrap();
    let conflicting = ExecutionFinish {
        usage: Usage {
            output_tokens: Some(observed.output_tokens.unwrap_or(0) + 1),
            ..observed
        },
        ..record
    };
    assert_eq!(
        crate::governance::finish_accepted_job(&f.store, &conflicting).await,
        Err(InferenceError::Storage)
    );
    assert_eq!(amounts(f, id).await, a);
    let row: StoredUsage = sqlx::query_as(
        "SELECT input_tokens,output_tokens,billing_usage,meter_usage,provider_cost_microusd FROM governance_reservations WHERE execution_id=$1")
        .bind(id).fetch_one(&f.store.pool).await.unwrap();
    assert_eq!(
        row,
        (
            observed.input_tokens.map(|n| n as i64),
            observed.output_tokens.map(|n| n as i64),
            observed.billing.map(|b| serde_json::to_value(b).unwrap()),
            observed.meters.map(|m| serde_json::to_value(m).unwrap()),
            observed.provider_cost_microusd
        )
    );
    let execution: (Option<i64>, Option<i64>) =
        sqlx::query_as("SELECT input_tokens,output_tokens FROM inference_executions WHERE id=$1")
            .bind(id)
            .fetch_one(&f.store.pool)
            .await
            .unwrap();
    assert_eq!(execution, (row.0, row.1));
    let entries: Vec<(String, Option<i64>, Option<i64>)> = sqlx::query_as("SELECT kind,input_tokens,output_tokens FROM monetary_ledger WHERE execution_id=$1 AND kind<>'hold'")
        .bind(id).fetch_all(&f.store.pool).await.unwrap();
    assert_eq!(entries, vec![("unknown".into(), row.0, row.1)]);
    let evidence: (Option<Value>, Option<Value>, Option<i64>, Option<Value>) = sqlx::query_as(
        "SELECT billing_usage,meter_usage,provider_cost_microusd,cost_components FROM monetary_ledger WHERE execution_id=$1 AND kind='unknown'")
        .bind(id).fetch_one(&f.store.pool).await.unwrap();
    assert_eq!(evidence, (row.2, row.3, row.4, None));
    consistent(f).await;
}

#[dbtest::test]
async fn accepted_failed_video_preserves_unknown_usage(pool: PgPool) {
    use crate::jobs::db_tests::{self, Fake, upstream_video, video_request, video_setup};
    let f = fixture(pool).await;
    video_setup(&f).await;
    let fake = Fake::default();
    let jobs = db_tests::jobs(&f, &fake, false);
    // The NA-token price also must not invent observations for a failed job.
    for free in [true, false] {
        if free {
            price(&f, true).await;
        } else {
            video_setup(&f).await;
        }
        fake.push_video(upstream_video(JobState::Queued, Some(8)));
        let id = Uuid::new_v4();
        let job = jobs
            .create_video(f.principal, video_request(), id)
            .await
            .unwrap();
        let held = amounts(&f, id).await.1;
        fake.push_video(upstream_video(JobState::Failed, None));
        let failed = jobs.refresh_video(job.clone()).await.unwrap();
        assert_eq!(failed.state, "failed");
        assert!(failed.settled_at.is_some());
        // Stale settlement replay and terminal retrieval cannot append evidence.
        jobs.settle(
            &job,
            super::super::video::settlement(&job, &upstream_video(JobState::Failed, None)).unwrap(),
        )
        .await
        .unwrap();
        jobs.get_video(&f.principal, &client_id("video_", job.id))
            .await
            .unwrap();
        unknown(&f, id, Usage::default(), held).await;
    }
}

#[dbtest::test]
async fn accepted_failed_native_batch_preserves_unknown_usage(pool: PgPool) {
    let w = world(pool, true, false, 1).await;
    w.openai.script.lock().unwrap().unique_upstream_ids = true;
    for free in [true, false] {
        price(&w.f, free).await;
        for observed in [
            None,
            Some(Usage {
                output_tokens: None,
                provider_cost_microusd: Some(17),
                ..usage(3, 2)
            }),
            Some(usage(3, 2)),
            Some(usage(120, 2)),
        ] {
            let file = w
                .upload(&w.f.principal, &[chat_line("a", "company/smart", "one")])
                .await;
            let job = w
                .create(w.f.principal, &file, BatchEndpoint::ChatCompletions, None)
                .await
                .unwrap();
            assert_eq!(job.mode(), Some(BatchMode::Native));
            w.poll().await;
            let accepted = w.reload(&job).await;
            assert!(accepted.upstream_id.is_some());
            let held = amounts(&w.f, job.execution_id).await.1;
            let mut failed = upstream(BatchStatus::Failed);
            failed.id = accepted.upstream().unwrap();
            failed.usage = observed;
            w.openai.script.lock().unwrap().batches.push_back(failed);
            w.poll().await;
            assert!(w.reload(&job).await.settled_at.is_some());
            w.jobs
                .settle(
                    &accepted,
                    batch::settlement(JobState::Failed, observed).unwrap(),
                )
                .await
                .unwrap();
            w.poll().await;
            let expected_hold = if !free && observed.and_then(|u| u.input_tokens) == Some(120) {
                Some(held.unwrap().max(122))
            } else {
                held
            };
            unknown(
                &w.f,
                job.execution_id,
                observed.unwrap_or_default(),
                expected_hold,
            )
            .await;
            assert_eq!(
                amounts(&w.f, job.execution_id).await.3,
                observed.and_then(|u| u.input_tokens) == Some(120)
            );
        }
    }
}

#[dbtest::test]
async fn accepted_failed_legacy_batch_preserves_unknown_usage(pool: PgPool) {
    let w = world(pool, true, false, 1).await;
    sqlx::query("UPDATE models SET supported_protocols=ARRAY['batches'] WHERE id=$1")
        .bind(w.f.model)
        .execute(&w.f.store.pool)
        .await
        .unwrap();
    for free in [true, false] {
        price(&w.f, free).await;
        for observed in [
            None,
            Some(Usage {
                meters: Some(MeterUsage {
                    requests: Some(1),
                    ..MeterUsage::default()
                }),
                ..Usage::default()
            }),
            Some(usage(3, 2)),
        ] {
            let id = Uuid::new_v4();
            let (target, _) = w
                .jobs
                .select(
                    &w.f.principal,
                    "company/smart",
                    ApiProtocol::Batches,
                    id,
                    |_, _| true,
                )
                .await
                .unwrap();
            let attempt = w
                .jobs
                .admit(
                    w.f.principal,
                    id,
                    "company/smart",
                    &target,
                    &WorkloadAdmission {
                        kind: WorkloadKind::Batches,
                        output: OutputReservation::Batch {
                            requests: 1,
                            output_tokens: 10,
                            max_line_output: 10,
                        },
                        unit_ceilings: MeterUsage {
                            requests: Some(1),
                            ..MeterUsage::default()
                        },
                    },
                )
                .await
                .unwrap();
            let upstream_id =
                UpstreamId::parse(&format!("legacy_{}", Uuid::new_v4().simple())).unwrap();
            let new = store::NewJob {
                id: Uuid::new_v4(),
                kind: JobKind::Batch,
                workspace_id: w.f.principal.workspace_id,
                api_key_id: w.f.principal.key_id,
                deployment_id: target.id,
                execution_id: id,
                public_model: "company/smart",
                provider: "openai",
                upstream_id: &upstream_id,
                poll_deadline_at: UtcInstant::now() + TimeDelta::hours(24),
                video_seconds: None,
                video_size: None,
                batch_endpoint: Some("/v1/chat/completions"),
            };
            let job = crate::governance::jobs::accept_job(
                &w.f.store,
                id,
                new.poll_deadline_at + LEASE_GRACE,
                async |conn| {
                    store::insert_job(conn, &new)
                        .await
                        .map_err(|_| InferenceError::Storage)
                },
            )
            .await
            .unwrap();
            attempt.accepted();
            let held = amounts(&w.f, id).await.1;
            let mut failed = upstream(BatchStatus::Failed);
            failed.id = job.upstream().unwrap();
            failed.usage = observed;
            w.openai.script.lock().unwrap().batches.push_back(failed);
            let terminal = w.jobs.refresh_legacy(job.clone()).await.unwrap();
            assert_eq!(terminal.state, "failed");
            assert!(terminal.settled_at.is_some());
            w.jobs
                .settle(&job, batch::settlement(JobState::Failed, observed).unwrap())
                .await
                .unwrap();
            unknown(&w.f, id, observed.unwrap_or_default(), held).await;
        }
    }
}

#[dbtest::test]
async fn native_submission_rejection_preserves_free_zero(pool: PgPool) {
    let w = world(pool, true, false, 1).await;
    price(&w.f, true).await;
    w.openai.script.lock().unwrap().submission_error = Some(InferenceError::UpstreamRejected);
    let file = w
        .upload(&w.f.principal, &[chat_line("a", "company/smart", "one")])
        .await;
    let job = w
        .create(w.f.principal, &file, BatchEndpoint::ChatCompletions, None)
        .await
        .unwrap();
    w.poll().await;
    let terminal = w.reload(&job).await;
    assert_eq!(terminal.state, "failed");
    assert!(terminal.upstream_id.is_none());
    assert_eq!(
        amounts(&w.f, job.execution_id).await,
        ("settled".into(), Some(0), Some(0), false)
    );
}

#[dbtest::test]
async fn native_acceptance_survives_a_concurrent_encoder_failure(pool: PgPool) {
    let w = world(pool, true, false, 1).await;
    price(&w.f, true).await;
    let file = w
        .upload(&w.f.principal, &[chat_line("a", "company/smart", "one")])
        .await;
    let job = w
        .create(w.f.principal, &file, BatchEndpoint::ChatCompletions, None)
        .await
        .unwrap();
    // The provider returns an accepted id while the local encoder fails.
    // The accepted response is stronger evidence than the error's class.
    {
        let mut script = w.openai.script.lock().unwrap();
        script.early_accept = true;
        script.encode_error = Some(InferenceError::UpstreamRejected);
    }
    w.poll().await;
    unknown(&w.f, job.execution_id, Usage::default(), Some(0)).await;
    let terminal = w.reload(&job).await;
    assert_eq!(terminal.state, "failed");
    assert_eq!(terminal.upstream_id.as_deref(), Some("batch_native1"));
}

mod http_tests;
