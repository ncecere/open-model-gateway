//! Pure job-service units: configuration, settlement mapping, rendering.
use super::*;
use crate::billing::MeterVariant;

#[test]
fn limits_parse_strictly() {
    let env = |pairs: &'static [(&'static str, &'static str)]| {
        move |k: &str| {
            pairs
                .iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| v.to_string())
        }
    };
    assert_eq!(
        JobLimits::from_lookup(env(&[])).unwrap(),
        JobLimits::default()
    );
    let l = JobLimits::from_lookup(env(&[
        ("GATEWAY_JOB_POLL_INTERVAL_SECONDS", "0"),
        ("GATEWAY_MAX_BATCH_FILE_BYTES", "1048576"),
    ]))
    .unwrap();
    assert_eq!(l.poll_interval, None);
    assert_eq!(l.batch_file_bytes, 1_048_576);
    assert_eq!((l.batch_workers, l.batch_concurrency), (4, 2));
    let l = JobLimits::from_lookup(env(&[
        ("GATEWAY_BATCH_WORKERS", "0"),
        ("GATEWAY_BATCH_CONCURRENCY", "8"),
    ]))
    .unwrap();
    assert_eq!((l.batch_workers, l.batch_concurrency), (0, 8));
    for bad in [
        ("GATEWAY_JOB_POLL_INTERVAL_SECONDS", "3601"),
        ("GATEWAY_JOB_POLL_INTERVAL_SECONDS", "-1"),
        ("GATEWAY_MAX_BATCH_FILE_BYTES", "1"),
        ("GATEWAY_MAX_BODY_BYTES_VIDEOS", "x"),
        ("GATEWAY_BATCH_MAX_OUTPUT_SCAN_BYTES", "10"),
        ("GATEWAY_BATCH_WORKERS", "257"),
        ("GATEWAY_BATCH_CONCURRENCY", "0"),
    ] {
        let pairs: &'static [(&str, &str)] = Box::leak(Box::new([bad]));
        assert!(JobLimits::from_lookup(env(pairs)).is_err(), "{bad:?}");
    }
}

fn row(state: &str) -> JobRow {
    JobRow {
        id: Uuid::new_v4(),
        kind: "video".into(),
        workspace_id: Uuid::new_v4(),
        api_key_id: Uuid::new_v4(),
        deployment_id: Uuid::new_v4(),
        execution_id: Uuid::new_v4(),
        public_model: "company/video".into(),
        provider: "openai".into(),
        upstream_id: Some("video_up".into()),
        state: state.into(),
        upstream_status: Some(state.into()),
        progress: None,
        error_code: Some("moderation_blocked".into()),
        created_at: Utc::now(),
        completed_at: None,
        expires_at: None,
        cancel_requested_at: None,
        deleted_at: None,
        poll_deadline_at: Utc::now(),
        settled_at: None,
        video_seconds: Some(8),
        video_size: Some("720x1280".into()),
        batch_endpoint: None,
        request_total: None,
        request_completed: None,
        request_failed: None,
        batch_mode: None,
        user_id: None,
        input_file_id: None,
        work_file_id: None,
        output_file_id: None,
        error_file_id: None,
        price_tier: None,
        retry_limit: 0,
        submit_started_at: None,
        in_progress_at: None,
        finalizing_at: None,
        last_progress_at: None,
        completion_window_hours: None,
    }
}
fn upstream(state: JobState, seconds: Option<u32>) -> UpstreamVideo {
    UpstreamVideo {
        id: UpstreamId::parse("video_up").unwrap(),
        state,
        progress: Some(100),
        seconds,
        size: MeterVariant::new("1792x1024"),
        completed_at: None,
        expires_at: None,
        error: None,
    }
}

#[test]
fn video_settlement_is_exact_or_unknown() {
    let job = row("completed");
    let s = video::settlement(&job, &upstream(JobState::Completed, Some(8))).unwrap();
    assert_eq!(s.outcome, Outcome::Succeeded);
    let m = s.usage.meters.unwrap();
    assert_eq!(m.output_video_seconds_ms, Some(8000));
    assert_eq!((m.requests, m.output_images), (Some(1), Some(0)));
    // The reported resolution is the billed variant.
    assert_eq!(s.usage.output_image_variant.unwrap().as_str(), "1792x1024");
    // Unknown duration: settled as unknown (hold kept), never zero.
    let s = video::settlement(&job, &upstream(JobState::Completed, None)).unwrap();
    assert_eq!(s.usage.meters.unwrap().output_video_seconds_ms, None);
    // Longer than requested: evidence kept, attempt failed (hold kept).
    let s = video::settlement(&job, &upstream(JobState::Completed, Some(12))).unwrap();
    assert_eq!(
        (s.outcome, s.error),
        (Outcome::Failed, Some(InferenceError::InvalidUpstream))
    );
    let s = video::settlement(&job, &upstream(JobState::Failed, None)).unwrap();
    assert_eq!(s.outcome, Outcome::Failed);
    assert_eq!(s.usage, Usage::default());
    assert!(video::settlement(&job, &upstream(JobState::InProgress, None)).is_none());
}

#[test]
fn batch_settlement_records_evidence_and_keeps_holds() {
    use batch::settlement;
    let u = Usage {
        input_tokens: Some(10),
        output_tokens: Some(2),
        ..Usage::default()
    };
    assert_eq!(
        settlement(JobState::Completed, Some(u)).unwrap().outcome,
        Outcome::Succeeded
    );
    // Completed without usage: succeeded but unknown (no counters).
    assert_eq!(
        settlement(JobState::Completed, None).unwrap().usage,
        Usage::default()
    );
    let c = settlement(JobState::Cancelled, Some(u)).unwrap();
    assert_eq!((c.outcome, c.usage), (Outcome::Cancelled, u));
    let e = settlement(JobState::Expired, Some(u)).unwrap();
    assert_eq!(e.error, Some(InferenceError::Timeout));
    assert!(settlement(JobState::Queued, None).is_none());
}

#[test]
fn rendering_uses_gateway_ids_and_no_content() {
    let job = row("failed");
    let v = video::render(&job);
    assert_eq!(v["id"], client_id("video_", job.id));
    assert_eq!(v["object"], "video");
    assert_eq!(v["status"], "failed");
    assert_eq!(v["seconds"], "8");
    assert_eq!(v["error"]["code"], "moderation_blocked");
    let text = v.to_string();
    assert!(!text.contains("video_up"), "upstream id leaked");
    assert!(v.get("prompt").is_none());
    let mut b = row("in_progress");
    b.kind = "batch".into();
    b.batch_endpoint = Some(BatchEndpoint::Responses.as_str().into());
    b.batch_mode = Some("gateway".into());
    b.upstream_status = Some("finalizing".into());
    b.request_total = Some(3);
    let input = Uuid::new_v4();
    b.input_file_id = Some(input);
    let v = batch::render_batch(&b, None);
    assert_eq!(v["status"], "finalizing");
    assert_eq!(v["endpoint"], "/v1/responses");
    assert_eq!(v["input_file_id"], client_id(FILE_PREFIX, input));
    assert_eq!(v["request_counts"]["total"], 3);
    assert!(v["metadata"].is_null());
    assert!(!v.to_string().contains("video_up"));
    // Terminal rows show their state; legacy rows their 0016 file ids.
    let mut done = b.clone();
    done.state = "cancelled".into();
    done.completed_at = Some(Utc::now());
    let legacy = Uuid::new_v4();
    let v = batch::render_batch(&done, Some((Some(legacy), None, None)));
    assert_eq!(v["status"], "cancelled");
    assert!(v["cancelled_at"].is_number());
    assert_eq!(v["input_file_id"], client_id(FILE_PREFIX, legacy));
}
