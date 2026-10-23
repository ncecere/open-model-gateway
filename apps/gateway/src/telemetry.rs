//! Bounded, typed OTLP/HTTP JSON traces. No automatic tracing-event export.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Environment, TraceConfig};
    use serde_json::{Value, json};
    use std::{sync::{Arc, Mutex}, time::{Duration, Instant}};
    use uuid::Uuid;

    fn config(values: &[(&str, &str)]) -> anyhow::Result<TraceConfig> {
        TraceConfig::from_lookup(|key| values.iter().find(|(k, _)| *k == key).map(|(_, v)| (*v).into()), Environment::Development)
    }

    #[test]
    fn configuration_refuses_invalid_values_without_echoing_them() {
        assert!(config(&[]).unwrap().endpoint.is_none());
        assert_eq!(config(&[]).unwrap().sample_ratio, 0.01);
        assert!(config(&[("OTEL_EXPORTER_OTLP_ENDPOINT", "bad")]).unwrap().endpoint.is_none());
        for ratio in ["NaN", "inf", "-0.1", "1.1", "secret-ratio"] {
            let error = config(&[("GATEWAY_TRACES_SAMPLE_RATIO", ratio)]).err().unwrap().to_string();
            assert!(!error.contains(ratio));
        }
        for url in ["https://user:secret@example.com/v1/traces", "https://example.com/v1/traces?secret", "https://example.com/v1/traces#secret", "http://example.com/v1/traces", "https://169.254.169.254/v1/traces", "https://[fd00:ec2::254]/v1/traces", "https://10.1.1.1/v1/traces", "https://example.com/v1/logs", "not-a-url"] {
            let error = config(&[("GATEWAY_TRACES_OTLP_URL", url)]).err().unwrap().to_string();
            assert!(!error.contains(url));
        }
        for header in ["Content-Type=secret", "Host=secret", "X=secret\r\nX:y", "bad-secret", "X="] {
            let error = config(&[("GATEWAY_TRACES_OTLP_URL", "https://example.com/v1/traces"), ("GATEWAY_TRACES_OTLP_HEADERS_ENV", "TRACE_SECRET"), ("TRACE_SECRET", header)]).err().unwrap().to_string();
            assert!(!error.contains("secret"));
        }
        assert!(config(&[("GATEWAY_TRACES_OTLP_HEADERS_ENV", "TRACE_SECRET")]).is_err());
        for reference in ["GATEWAY_SECRET_ENV_ALLOWLIST", "GATEWAY_SMTP_SECRET_ENV_ALLOWLIST", "GATEWAY_AUDIT_OTLP_HEADERS_ENV", "GATEWAY_AUDIT_WEBHOOK_SECRET_ENV"] {
            assert!(config(&[("GATEWAY_TRACES_OTLP_URL", "https://example.com/v1/traces"), ("GATEWAY_TRACES_OTLP_HEADERS_ENV", "TRACE_SECRET"), ("TRACE_SECRET", "Authorization=Bearer secret"), (reference, "TRACE_SECRET")]).is_err());
        }
        let large = format!("X={}", "x".repeat(4096));
        assert!(config(&[("GATEWAY_TRACES_OTLP_URL", "https://example.com/v1/traces"), ("GATEWAY_TRACES_OTLP_HEADERS_ENV", "TRACE_SECRET"), ("TRACE_SECRET", &large)]).is_err());
        let many = (0..17).map(|i| format!("X-{i}=x")).collect::<Vec<_>>().join(",");
        assert!(config(&[("GATEWAY_TRACES_OTLP_URL", "https://example.com/v1/traces"), ("GATEWAY_TRACES_OTLP_HEADERS_ENV", "TRACE_SECRET"), ("TRACE_SECRET", &many)]).is_err());
    }

    #[test]
    fn sampling_and_trace_ids_are_deterministic() {
        let id = Uuid::from_u128(42);
        assert_eq!(trace_id(id), trace_id(id));
        assert_ne!(trace_id(id), trace_id(Uuid::from_u128(43)));
        assert_ne!(trace_id(id), [0; 16]);
        assert_ne!(trace_id(id), *id.as_bytes());
        let sampled = (1..=10000).filter(|n| root_sampled(Uuid::from_u128(*n), 0.01)).count();
        assert!((60..=140).contains(&sampled), "{sampled}");
        for n in 1..100 {
            let id = Uuid::from_u128(n);
            assert!(!root_sampled(id, 0.0));
            assert!(root_sampled(id, 1.0));
            assert_eq!(root_sampled(id, 0.3), root_sampled(id, 0.3));
        }
    }

    fn span() -> Span {
        Span::new(SpanName::InferenceRequest, trace_id(Uuid::from_u128(42)), None, 1_700_000_000_000_000_001, 1_700_000_000_000_000_002).unwrap()
    }

    #[test]
    fn typed_attributes_and_limits_never_export_arbitrary_fields() {
        let mut span = span();
        span.attribute(Attribute::Operation(Operation::Chat));
        span.attribute(Attribute::InputTokens(42));
        span.attribute(Attribute::Streamed(true));
        let body = serde_json::to_value(&span).unwrap();
        assert_eq!(body["name"], "inference.request");
        assert_eq!(body["attributes"][1], json!({"key":"gen_ai.usage.input_tokens","value":{"intValue":"42"}}));
        let mut aliases = AliasRegistry::default();
        assert_eq!(aliases.admission_validated("a").as_str(), "a");
        assert_eq!(aliases.admission_validated(&"x".repeat(97)).as_str(), "other");
        for n in 0..200 { aliases.admission_validated(&format!("model-{n}")); }
        assert_eq!(aliases.admission_validated("new-value").as_str(), "other");
        for _ in 0..25 { span.attribute(Attribute::InputTokens(i64::MAX as u64)); }
        assert!(span.attributes.len() <= 24);
        assert!(serde_json::to_vec(&span).unwrap().len() <= 1024);
        let text = serde_json::to_string(&span).unwrap();
        for forbidden in ["prompt", "response", "workspace", "user", "secret", "hostname", "exception"] { assert!(!text.contains(forbidden)); }
    }

    #[tokio::test]
    async fn contexts_queue_and_span_budgets_are_nonblocking_and_count_drops() {
        let (recorder, mut rx) = Recorder::channel(1.0);
        let contexts = (0..256).map(|_| recorder.begin(Uuid::new_v4()).unwrap()).collect::<Vec<_>>();
        assert!(recorder.begin(Uuid::new_v4()).is_none());
        assert_eq!(recorder.counts.dropped(DropReason::Contexts), 1);
        drop(contexts);
        let mut context = recorder.begin(Uuid::new_v4()).unwrap();
        for _ in 0..65 { context.record(span()); }
        assert_eq!(recorder.counts.dropped(DropReason::RequestLimit), 1);
        // Unknown or failed eligibility never exports buffered observations.
        drop(context);
        assert!(rx.try_recv().is_err());
        for _ in 0..32 {
            let mut context = recorder.begin(Uuid::new_v4()).unwrap();
            for _ in 0..64 { context.record(span()); }
            context.export(SharedAdmission(()));
        }
        assert_eq!(rx.len(), 2048);
        let started = Instant::now();
        let mut context = recorder.begin(Uuid::new_v4()).unwrap();
        context.record(span());
        context.export(SharedAdmission(()));
        assert!(started.elapsed() < Duration::from_millis(100));
        assert_eq!(recorder.counts.dropped(DropReason::QueueFull), 1);
        rx.close();
    }

    async fn collector(reply: &'static str, delay: Duration) -> (String, Arc<Mutex<Vec<Value>>>, tokio::task::JoinHandle<()>) {
        use axum::{Router, routing::post, extract::State, Json};
        type Seen = Arc<Mutex<Vec<Value>>>;
        let seen: Seen = Arc::default();
        let app = Router::new().route("/v1/traces", post(move |State(seen): State<Seen>, Json(body): Json<Value>| async move {
            seen.lock().unwrap().push(body);
            tokio::time::sleep(delay).await;
            ([("content-type", "application/json")], reply)
        })).with_state(seen.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1/traces", listener.local_addr().unwrap());
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap(); });
        (url, seen, task)
    }

    fn runtime(url: &str) -> Runtime {
        Runtime::start(config(&[("GATEWAY_TRACES_OTLP_URL", url), ("GATEWAY_TRACES_SAMPLE_RATIO", "1")]).unwrap()).unwrap()
    }

    fn emit(recorder: &Recorder, n: usize) {
        let mut context = recorder.begin(Uuid::new_v4()).unwrap();
        for _ in 0..n { context.record(span()); }
        context.export(SharedAdmission(()));
    }

    #[tokio::test]
    async fn mock_collector_receives_exact_otlp_shape_and_partial_rejections() {
        let (url, seen, task) = collector(r#"{"partialSuccess":{"rejectedSpans":"1","errorMessage":"SECRET must not be logged"}}"#, Duration::ZERO).await;
        let runtime = runtime(&url);
        let recorder = runtime.recorder();
        emit(&recorder, 2);
        runtime.shutdown().await;
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        let body = &seen[0];
        assert!(serde_json::to_vec(body).unwrap().len() <= 64 * 1024);
        assert_eq!(body["resourceSpans"][0]["resource"]["attributes"], json!([
            {"key":"service.name","value":{"stringValue":"open-model-gateway"}},
            {"key":"service.version","value":{"stringValue":env!("CARGO_PKG_VERSION")}},
            {"key":"deployment.environment.name","value":{"stringValue":"development"}}
        ]));
        let spans = body["resourceSpans"][0]["scopeSpans"][0]["spans"].as_array().unwrap();
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0]["traceId"].as_str().unwrap().len(), 32);
        assert_eq!(spans[0]["spanId"].as_str().unwrap().len(), 16);
        assert!(hex::decode(spans[0]["traceId"].as_str().unwrap()).is_ok());
        assert_eq!(spans[0]["startTimeUnixNano"], "1700000000000000001");
        assert_eq!(spans[0]["endTimeUnixNano"], "1700000000000000002");
        assert_eq!(recorder.counts.rejected(), 1);
        assert_eq!(recorder.counts.deliveries(Delivery::Accepted), 1);
        task.abort();
    }

    #[tokio::test]
    async fn slow_collector_and_shutdown_are_bounded_and_never_block_recording() {
        let (url, _, task) = collector("{}", Duration::from_secs(30)).await;
        let runtime = runtime(&url);
        let recorder = runtime.recorder();
        let started = Instant::now();
        for _ in 0..100 { emit(&recorder, 64); }
        assert!(started.elapsed() < Duration::from_secs(1));
        let started = Instant::now();
        runtime.shutdown().await;
        assert!(started.elapsed() < Duration::from_millis(3300));
        assert!(recorder.counts.deliveries(Delivery::Timeout) > 0);
        assert!(recorder.counts.dropped(DropReason::Shutdown) > 0);
        task.abort();
    }

    #[tokio::test]
    async fn down_collector_is_not_retried_and_https_never_falls_back_to_plaintext() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1/traces", listener.local_addr().unwrap());
        drop(listener);
        let runtime = runtime(&url);
        let recorder = runtime.recorder();
        emit(&recorder, 1);
        runtime.shutdown().await;
        assert_eq!(recorder.counts.deliveries(Delivery::Transport), 1);
        let (url, seen, task) = collector("{}", Duration::ZERO).await;
        let runtime = self::runtime(&url.replacen("http:", "https:", 1));
        let recorder = runtime.recorder();
        emit(&recorder, 1);
        runtime.shutdown().await;
        assert_eq!(recorder.counts.deliveries(Delivery::Transport), 1);
        assert!(seen.lock().unwrap().is_empty());
        task.abort();
    }
}
