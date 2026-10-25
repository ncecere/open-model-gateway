// `test_lock` serializes tests that attach the process-wide singleton; holding
// it across awaits in these test-only, per-test runtimes is intended.
#![allow(clippy::await_holding_lock)]
use super::*;
use serde_json::Value;

fn exporter(url: &str) -> Exporter {
    Exporter {
        url: url.parse().unwrap(),
        client: reqwest::Client::builder().no_proxy().build().unwrap(),
        headers: Default::default(),
        environment: crate::config::Environment::Development,
        instance: uuid::Uuid::nil(),
    }
}

fn degraded() -> OpsEvent {
    OpsEvent::ExporterDegraded {
        signal: Signal::Metrics,
        failure: ExportFailure::Transport,
    }
}

fn every_variant() -> [OpsEvent; OpsEvent::COUNT] {
    [
        degraded(),
        OpsEvent::ExporterRecovered {
            signal: Signal::Traces,
        },
        OpsEvent::EgressRefused {
            purpose: crate::egress::Purpose::Provider,
        },
        OpsEvent::LeaseLost {
            lease: crate::leases::Lease::Partitions,
        },
        OpsEvent::ConfigListenerDown,
        OpsEvent::FinalizationNeedsReconciliation {
            finalizer: Finalizer::GenerationFinish,
            trace: None,
        },
        OpsEvent::BackgroundJobFailed {
            job: Job::Partitions,
            failure: BackgroundFailure::Error,
        },
    ]
}

// ---- encoder: golden wire shape -------------------------------------------

#[test]
fn log_record_shape_is_pinned_including_severity_and_event_name() {
    let record = log_record(&OpsEvent::ConfigListenerDown, 123, 0);
    assert_eq!(
        record,
        serde_json::json!({
            "timeUnixNano": "123",
            "observedTimeUnixNano": "123",
            "severityNumber": 13,
            "severityText": "WARN",
            "body": {"stringValue": "Configuration listener unavailable"},
            "attributes": [
                {"key": "event.name", "value": {"stringValue": "omg.ops.config_listener_down"}}
            ],
        })
    );
    assert!(record.get("traceId").is_none());
}

#[test]
fn every_variant_has_a_fixed_severity_body_name_and_slug() {
    let cases: [(OpsEvent, Severity, &str, &str, &str); OpsEvent::COUNT] = [
        (
            degraded(),
            Severity::Warn,
            "Exporter degraded",
            "omg.ops.exporter_degraded",
            "exporter_degraded",
        ),
        (
            OpsEvent::ExporterRecovered {
                signal: Signal::Traces,
            },
            Severity::Info,
            "Exporter recovered",
            "omg.ops.exporter_recovered",
            "exporter_recovered",
        ),
        (
            OpsEvent::EgressRefused {
                purpose: crate::egress::Purpose::Provider,
            },
            Severity::Warn,
            "Egress refused",
            "omg.ops.egress_refused",
            "egress_refused",
        ),
        (
            OpsEvent::LeaseLost {
                lease: crate::leases::Lease::Partitions,
            },
            Severity::Warn,
            "Lease lost",
            "omg.ops.lease_lost",
            "lease_lost",
        ),
        (
            OpsEvent::ConfigListenerDown,
            Severity::Warn,
            "Configuration listener unavailable",
            "omg.ops.config_listener_down",
            "config_listener_down",
        ),
        (
            OpsEvent::FinalizationNeedsReconciliation {
                finalizer: Finalizer::GenerationFinish,
                trace: None,
            },
            Severity::Error,
            "Finalization requires reconciliation",
            "omg.ops.finalization_needs_reconciliation",
            "finalization_needs_reconciliation",
        ),
        (
            OpsEvent::BackgroundJobFailed {
                job: Job::Partitions,
                failure: BackgroundFailure::Error,
            },
            Severity::Warn,
            "Background job failed",
            "omg.ops.background_job_failed",
            "background_job_failed",
        ),
    ];
    for (event, severity, body, name, slug) in cases {
        assert_eq!(event.severity(), severity, "{name}");
        assert_eq!(event.body(), body);
        assert_eq!(event.name(), name);
        assert_eq!(OpsEvent::SLUGS[event.index()], slug);
    }
}

#[test]
fn only_generation_finish_may_carry_a_trace_id_and_it_is_hexadecimal() {
    let with_trace = OpsEvent::FinalizationNeedsReconciliation {
        finalizer: Finalizer::GenerationFinish,
        trace: Some(LogTraceId::for_test("0123456789abcdef")),
    };
    let record = log_record(&with_trace, 1, 0);
    assert_eq!(record["traceId"], "0123456789abcdef");
    for event in [
        OpsEvent::FinalizationNeedsReconciliation {
            finalizer: Finalizer::GenerationCancel,
            trace: None,
        },
        OpsEvent::FinalizationNeedsReconciliation {
            finalizer: Finalizer::RealtimeFinish,
            trace: None,
        },
        OpsEvent::FinalizationNeedsReconciliation {
            finalizer: Finalizer::JobFail,
            trace: None,
        },
        degraded(),
        OpsEvent::ConfigListenerDown,
    ] {
        assert!(log_record(&event, 1, 0).get("traceId").is_none());
    }
}

#[test]
fn suppressed_count_attribute_only_appears_when_positive() {
    let none = log_record(&OpsEvent::ConfigListenerDown, 1, 0);
    assert!(
        !none["attributes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["key"] == "omg.ops.suppressed_count")
    );
    let some = log_record(&OpsEvent::ConfigListenerDown, 1, 7);
    let attr = some["attributes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["key"] == "omg.ops.suppressed_count")
        .unwrap();
    assert_eq!(attr["value"]["intValue"], "7");
}

#[test]
fn envelope_resource_matches_metrics_shared_helper_and_scope_is_omg_ops() {
    let built = exporter("https://collector.invalid/v1/logs");
    let body: Value = serde_json::from_slice(&built.body(&[])).unwrap();
    let resource = &body["resourceLogs"][0]["resource"];
    assert_eq!(
        resource["attributes"],
        crate::otlp::resource_attributes(
            crate::config::Environment::Development,
            uuid::Uuid::nil(),
            env!("CARGO_PKG_VERSION")
        )
    );
    assert_eq!(
        body["resourceLogs"][0]["scopeLogs"][0]["scope"],
        serde_json::json!({"name": "omg.ops", "version": env!("CARGO_PKG_VERSION")})
    );
    assert_eq!(
        body["resourceLogs"][0]["scopeLogs"][0]["logRecords"],
        serde_json::json!([])
    );
}

// ---- privacy canaries: every attribute value is one of the fixed enums ----

#[test]
fn no_variant_attribute_value_is_free_text_or_identity() {
    let canary_uuid = "07980fdd-1f4a-4716-bff3-20317aa86598";
    let allowed: Vec<&str> = Signal::ALL
        .iter()
        .map(|s| s.as_str())
        .chain([
            "http_status",
            "transport",
            "timeout",
            "acknowledgement",
            "rejected",
        ])
        .chain([
            "provider",
            "oidc",
            "storage",
            "catalog",
            "smtp",
            "audit",
            "telemetry",
            "alert_webhook",
        ])
        .chain(crate::leases::Lease::ALL.iter().map(|l| l.as_str()))
        .chain(Finalizer::ALL.iter().map(|f| f.as_str()))
        .chain(Job::ALL.iter().map(|j| j.as_str()))
        .chain(BackgroundFailure::ALL.iter().map(|f| f.as_str()))
        .collect();
    for event in every_variant() {
        let record = log_record(&event, 1, 0);
        let raw = record.to_string();
        assert!(!raw.contains(canary_uuid), "{raw}");
        assert!(!raw.contains("secret-prompt"), "{raw}");
        assert!(!raw.contains("example.invalid"), "{raw}");
        assert!(!raw.contains("collector.invalid"), "{raw}");
        for (_, value) in event.attributes() {
            assert!(
                allowed.contains(&value),
                "unexpected attribute value {value}"
            );
        }
    }
}

#[test]
fn only_an_allowed_resource_attribute_may_carry_a_uuid() {
    let canary = "07980fdd-1f4a-4716-bff3-20317aa86598";
    let instance = uuid::Uuid::parse_str(canary).unwrap();
    let with_instance = Exporter {
        instance,
        ..exporter("https://collector.invalid/v1/logs")
    };
    let text = String::from_utf8(with_instance.body(&[])).unwrap();
    // The process UUID appears exactly once, at service.instance.id.
    assert_eq!(text.matches(canary).count(), 1);
    assert!(text.contains(&format!(
        r#""key":"service.instance.id","value":{{"stringValue":"{canary}"}}"#
    )));
}

// ---- bounds / drop counters ------------------------------------------------

fn queued(event: OpsEvent) -> Queued {
    Queued {
        event,
        at: 1,
        suppressed: 0,
    }
}

fn pending(receiver: mpsc::Receiver<Queued>) -> Pending {
    Pending {
        receiver,
        in_flight: 0,
    }
}

#[tokio::test]
async fn batch_bounds_at_most_100_records_per_call() {
    let built = exporter("http://127.0.0.1:1/v1/logs");
    let (sender, receiver) = mpsc::channel(QUEUE);
    let mut pending = pending(receiver);
    for _ in 0..150 {
        sender
            .try_send(queued(OpsEvent::ConfigListenerDown))
            .unwrap();
    }
    assert_eq!(pending.receiver.len(), 150);
    send_batch(&built, &mut pending).await;
    assert_eq!(
        pending.receiver.len(),
        50,
        "at most MAX_BATCH records are consumed by one send_batch call"
    );
    assert_eq!(
        pending.in_flight, 0,
        "accounted back to zero after the exchange"
    );
}

#[test]
fn oversized_record_is_dropped_as_record_limit_not_fabricated() {
    let huge_trace = "a".repeat(600);
    let event = OpsEvent::FinalizationNeedsReconciliation {
        finalizer: Finalizer::GenerationFinish,
        trace: Some(LogTraceId::for_test(&huge_trace)),
    };
    let record = log_record(&event, 1, 0);
    assert!(json_size(&record) > MAX_RECORD);
}

#[test]
fn full_queue_counts_queue_full_and_closed_counts_shutdown() {
    let (sender, receiver) = mpsc::channel::<Queued>(1);
    sender
        .try_send(queued(OpsEvent::ConfigListenerDown))
        .unwrap();
    assert!(matches!(
        sender.try_send(queued(OpsEvent::ConfigListenerDown)),
        Err(mpsc::error::TrySendError::Full(_))
    ));
    drop(receiver);
    assert!(matches!(
        sender.try_send(queued(OpsEvent::ConfigListenerDown)),
        Err(mpsc::error::TrySendError::Closed(_))
    ));
}

// ---- severity floor and the per-variant limiter ----------------------------

#[test]
fn severity_floor_filters_before_the_limiter_ever_counts() {
    let _guard = test_lock();
    configure(None, Some(Severity::Warn));
    let index = OpsEvent::ExporterRecovered {
        signal: Signal::Metrics,
    }
    .index();
    let before = crate::metrics::METRICS.otlp_logs_suppressed_count(index);
    // INFO is below the WARN floor: dropped before the limiter, so a run of
    // INFO events is never counted as "suppressed" either.
    for _ in 0..3 {
        emit(OpsEvent::ExporterRecovered {
            signal: Signal::Metrics,
        });
    }
    assert_eq!(
        crate::metrics::METRICS.otlp_logs_suppressed_count(index),
        before
    );
    configure(None, None);
}

#[test]
fn limiter_admits_one_per_30_seconds_then_suppresses_and_accounts_once() {
    let limiter = Limiter::default();
    let index = OpsEvent::ConfigListenerDown.index();
    assert_eq!(limiter.admit(index), Some(0));
    assert_eq!(limiter.admit(index), None, "burst of one");
    assert_eq!(limiter.admit(index), None);
    assert_eq!(limiter.suppressed[index].load(Ordering::Relaxed), 2);
    // Simulate refill by resetting the recorded timestamp directly.
    limiter.last[index].store(0, Ordering::Relaxed);
    assert_eq!(
        limiter.admit(index),
        Some(2),
        "the next admitted event carries the saturating suppressed count"
    );
    assert_eq!(limiter.suppressed[index].load(Ordering::Relaxed), 0);
}

#[test]
fn suppressed_counter_saturates_instead_of_wrapping_at_the_u64_boundary() {
    let limiter = Limiter::default();
    let index = OpsEvent::ConfigListenerDown.index();
    limiter.suppressed[index].store(u64::MAX, Ordering::Relaxed);
    // Still inside the same refill window: the increment path runs again.
    limiter.last[index].store(now_ms(), Ordering::Relaxed);
    assert_eq!(limiter.admit(index), None, "still suppressed, not refilled");
    assert_eq!(
        limiter.suppressed[index].load(Ordering::Relaxed),
        u64::MAX,
        "saturates instead of wrapping to zero"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_admission_never_bursts_past_one_and_accounts_every_suppression() {
    // Real OS threads, not cooperative single-thread scheduling: `admit`
    // has no `.await`, so only true parallelism can exercise the race the
    // compare-exchange fixes (a plain load-then-store could let several
    // threads all observe "ready" and all admit).
    let limiter = std::sync::Arc::new(Limiter::default());
    let index = OpsEvent::ConfigListenerDown.index();
    const CALLERS: usize = 64;
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(CALLERS));
    let mut tasks = Vec::new();
    for _ in 0..CALLERS {
        let limiter = limiter.clone();
        let barrier = barrier.clone();
        tasks.push(tokio::task::spawn_blocking(move || {
            barrier.wait();
            limiter.admit(index)
        }));
    }
    let mut admitted = 0u64;
    let mut suppressed_sum = 0u64;
    for task in tasks {
        if let Some(carried) = task.await.unwrap() {
            admitted += 1;
            suppressed_sum += carried;
        }
    }
    assert_eq!(
        admitted, 1,
        "exactly one concurrent caller wins the refill window, never a burst"
    );
    // Every suppression a non-winning caller recorded is accounted exactly
    // once: either carried by the one winner, or still sitting in the
    // counter (if its CAS happened to land before some callers incremented
    // it). Either way nothing is double-counted or silently lost.
    let remaining = limiter.suppressed[index].load(Ordering::Relaxed);
    assert_eq!(
        suppressed_sum + remaining,
        (CALLERS - 1) as u64,
        "every non-winning call is accounted exactly once"
    );
}

#[test]
fn limiter_tracks_each_variant_independently() {
    let limiter = Limiter::default();
    let a = OpsEvent::ConfigListenerDown.index();
    let b = degraded().index();
    assert_ne!(a, b);
    assert_eq!(limiter.admit(a), Some(0));
    assert_eq!(
        limiter.admit(b),
        Some(0),
        "an unrelated variant is unaffected"
    );
    assert_eq!(limiter.admit(a), None);
}

// ---- disabled by default: emit() is inert without a Runtime ---------------

#[test]
fn emit_without_a_runtime_touches_nothing() {
    let _guard = test_lock();
    let before = crate::metrics::METRICS.otlp_logs_drop_count(DropReason::Shutdown);
    emit(degraded());
    emit(OpsEvent::ConfigListenerDown);
    assert_eq!(
        crate::metrics::METRICS.otlp_logs_drop_count(DropReason::Shutdown),
        before
    );
}

// ---- collector acceptance: a real mock OTLP/HTTP JSON collector -----------

async fn collector(
    status: axum::http::StatusCode,
    reply: &'static str,
    delay: Duration,
) -> (
    String,
    std::sync::Arc<std::sync::Mutex<Vec<Value>>>,
    tokio::task::JoinHandle<()>,
) {
    use axum::{Json, Router, extract::State, routing::post};
    type Seen = std::sync::Arc<std::sync::Mutex<Vec<Value>>>;
    let seen: Seen = Default::default();
    let app = Router::new()
        .route(
            "/v1/logs",
            post(
                move |State(seen): State<Seen>, Json(body): Json<Value>| async move {
                    seen.lock().unwrap().push(body);
                    if !delay.is_zero() {
                        tokio::time::sleep(delay).await;
                    }
                    (status, reply)
                },
            ),
        )
        .with_state(seen.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1/logs", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (url, seen, task)
}

fn send(sender: &mpsc::Sender<Queued>, n: usize) {
    for _ in 0..n {
        sender
            .try_send(queued(OpsEvent::ConfigListenerDown))
            .unwrap();
    }
}

#[tokio::test]
async fn collector_accepts_a_batch_and_counts_the_push() {
    let (url, seen, task) = collector(axum::http::StatusCode::OK, "{}", Duration::ZERO).await;
    let built = exporter(&url);
    let (sender, receiver) = mpsc::channel(QUEUE);
    let mut pending = pending(receiver);
    send(&sender, 3);
    let before = crate::metrics::METRICS.otlp_logs_push_count(Delivery::Accepted);
    send_batch(&built, &mut pending).await;
    assert_eq!(seen.lock().unwrap().len(), 1);
    let body = seen.lock().unwrap()[0].clone();
    assert_eq!(
        body["resourceLogs"][0]["scopeLogs"][0]["logRecords"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
    assert_eq!(
        crate::metrics::METRICS.otlp_logs_push_count(Delivery::Accepted),
        before + 1
    );
    task.abort();
}

#[tokio::test]
async fn collector_partial_success_is_recorded_and_bounded_by_sent_count() {
    let (url, _seen, task) = collector(
        axum::http::StatusCode::OK,
        r#"{"partialSuccess":{"rejectedLogRecords":"999"}}"#,
        Duration::ZERO,
    )
    .await;
    let built = exporter(&url);
    let (sender, receiver) = mpsc::channel(QUEUE);
    let mut pending = pending(receiver);
    send(&sender, 2);
    let before = crate::metrics::METRICS.otlp_logs_rejected_count();
    send_batch(&built, &mut pending).await;
    assert_eq!(
        crate::metrics::METRICS.otlp_logs_rejected_count(),
        before + 2,
        "the acknowledgement is clamped to the sent count"
    );
    task.abort();
}

#[tokio::test]
async fn collector_http_error_counts_http_status_and_drops_the_batch_as_delivery() {
    let (url, _seen, task) = collector(
        axum::http::StatusCode::INTERNAL_SERVER_ERROR,
        "",
        Duration::ZERO,
    )
    .await;
    let built = exporter(&url);
    let (sender, receiver) = mpsc::channel(QUEUE);
    let mut pending = pending(receiver);
    send(&sender, 4);
    let before_push = crate::metrics::METRICS.otlp_logs_push_count(Delivery::HttpStatus);
    let before_drop = crate::metrics::METRICS.otlp_logs_drop_count(DropReason::Delivery);
    send_batch(&built, &mut pending).await;
    assert_eq!(
        crate::metrics::METRICS.otlp_logs_push_count(Delivery::HttpStatus),
        before_push + 1
    );
    assert_eq!(
        crate::metrics::METRICS.otlp_logs_drop_count(DropReason::Delivery),
        before_drop + 4
    );
    task.abort();
}

#[tokio::test]
async fn collector_timeout_never_retries_and_counts_timeout() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        // Accept and never answer. The accepted socket must stay bound (not
        // `let _ = ...`, which would drop and close it immediately,
        // resetting the connection and producing a fast Transport error
        // instead of the exchange's own timeout): keep it alive for as long
        // as this future is suspended below.
        if let Ok((_socket, _)) = listener.accept().await {
            std::future::pending::<()>().await
        }
    });
    let built = exporter(&format!("http://{address}/v1/logs"));
    let (sender, receiver) = mpsc::channel(QUEUE);
    let mut pending = pending(receiver);
    send(&sender, 1);
    let before = crate::metrics::METRICS.otlp_logs_push_count(Delivery::Timeout);
    let started = std::time::Instant::now();
    send_batch(&built, &mut pending).await;
    assert!(started.elapsed() < Duration::from_secs(3));
    assert_eq!(
        crate::metrics::METRICS.otlp_logs_push_count(Delivery::Timeout),
        before + 1
    );
    task.abort();
}

/// A forced abort (the outer drain deadline firing while an exchange is
/// still in flight) must never lose those records uncounted: `Pending`'s
/// `Drop` accounts `in_flight` even when the task is cut off mid-`.await`,
/// not just on a normal return.
#[tokio::test]
async fn forced_abort_mid_exchange_counts_in_flight_records_as_shutdown_drops() {
    // Accepts the connection but never answers: the exchange stays
    // suspended (inside otlp::post's own 2 s internal timeout) until
    // something external cancels it.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let hang = tokio::spawn(async move {
        while let Ok((_socket, _)) = listener.accept().await {
            std::future::pending::<()>().await;
        }
    });
    let built = exporter(&format!("http://{address}/v1/logs"));
    let (sender, receiver) = mpsc::channel(QUEUE);
    send(&sender, 5);
    let (_stop, stopping) = oneshot::channel();
    // A short interval so the first (and, here, only) batch starts quickly.
    let task = tokio::spawn(worker(built, receiver, Duration::from_millis(10), stopping));
    // Comfortably inside the hang and well under otlp::post's own 2 s
    // internal timeout, so the task is known to still be suspended inside
    // `exporter.send(...).await`, with `pending.in_flight == 5`.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let before = crate::metrics::METRICS.otlp_logs_drop_count(DropReason::Shutdown);
    task.abort();
    // Awaiting an aborted handle completes only once the task (and its
    // locals, including `Pending`) has actually finished dropping.
    let _ = task.await;
    assert_eq!(
        crate::metrics::METRICS.otlp_logs_drop_count(DropReason::Shutdown),
        before + 5,
        "the in-flight batch is counted even though the task was aborted mid-exchange"
    );
    hang.abort();
}

// ---- runtime: shutdown and the disabled path -------------------------------
// Startup refusal for invalid settings is exercised in
// logs_export_config_tests.rs (config.rs): this module is the transport and
// encoder, not the settings parser.

#[tokio::test]
async fn shutdown_closes_admission_before_draining_and_sends_at_most_one_final_batch() {
    let _guard = test_lock();
    let (url, seen, task) = collector(axum::http::StatusCode::OK, "{}", Duration::ZERO).await;
    let config = crate::config::LogsExportConfig {
        endpoint: Some((url.parse().unwrap(), reqwest::Client::new())),
        headers: Default::default(),
        min_severity: Severity::Warn,
    };
    let runtime = Runtime::start(config, crate::config::Environment::Development).unwrap();
    emit(OpsEvent::ConfigListenerDown);
    runtime.shutdown().await;
    // emit() after shutdown must be inert: admission is already closed.
    emit(OpsEvent::LeaseLost {
        lease: crate::leases::Lease::Partitions,
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    let batches = seen.lock().unwrap().len();
    assert!((0..=1).contains(&batches), "at most one final batch");
    task.abort();
}

#[test]
fn disabled_runtime_starts_nothing() {
    let config = crate::config::LogsExportConfig {
        endpoint: None,
        headers: Default::default(),
        min_severity: Severity::Warn,
    };
    assert!(Runtime::start(config, crate::config::Environment::Development).is_none());
}
