//! Short real-HTTP regressions for transport lifetime and reader backpressure.
use super::*;
use std::time::Instant;
use tokio::sync::{Notify, oneshot};

const READ: Duration = Duration::from_millis(200);
const TOTAL: Duration = Duration::from_millis(450);
const PAUSE: Duration = Duration::from_millis(1250);
fn timeouts() -> transport::Timeouts {
    transport::Timeouts {
        read: READ,
        total: TOTAL,
    }
}

struct Dropped(Option<oneshot::Sender<()>>);
impl Drop for Dropped {
    fn drop(&mut self) {
        if let Some(sender) = self.0.take() {
            let _ = sender.send(());
        }
    }
}
#[derive(Clone, Copy)]
enum Script {
    Healthy,
    Gated,
    Fragmented,
    Stalled,
    #[cfg(feature = "integration-tests")]
    Flowing,
}
struct TimedMock {
    base: String,
    gate: Arc<Notify>,
    dropped: oneshot::Receiver<()>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for TimedMock {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl TimedMock {
    async fn new(script: Script) -> Self {
        let gate = Arc::new(Notify::new());
        let notify = gate.clone();
        let (sender, dropped) = oneshot::channel();
        let sender = Arc::new(StdMutex::new(Some(sender)));
        let app = Router::new().fallback(move || {
            let gate = notify.clone();
            let guard = Dropped(sender.lock().unwrap().take());
            async move {
                let body = async_stream::stream! {
                    let _guard = guard;
                    let mut scripted = events(false);
                    // Explicit upstream evidence: these success cases have no
                    // cache usage, so the priced input/output cost is known.
                    let metadata = &mut scripted.last_mut().unwrap().1;
                    metadata["usage"]["cacheReadInputTokens"] = json!(0);
                    metadata["usage"]["cacheWriteInputTokens"] = json!(0);
                    let mut frames = scripted.into_iter();
                    // Start + first content arrive together; no timer should span this yield.
                    yield Ok::<_, Infallible>(Bytes::from(wire(frames.by_ref().take(2).collect())));
                    match script {
                        Script::Gated => gate.notified().await,
                        Script::Fragmented => {
                            gate.notified().await;
                            let tail = wire(frames.collect());
                            // Only a prefix is queued while the reader is idle.
                            yield Ok(Bytes::copy_from_slice(&tail[..1]));
                            gate.notified().await;
                            yield Ok(Bytes::copy_from_slice(&tail[1..]));
                            return;
                        },
                        Script::Stalled => std::future::pending::<()>().await,
                        Script::Healthy => {}
                        #[cfg(feature = "integration-tests")]
                        Script::Flowing => loop {
                            tokio::time::sleep(Duration::from_millis(100)).await;
                            yield Ok(Bytes::from(wire(vec![("contentBlockDelta", json!({"contentBlockIndex":0,"delta":{"text":"more"}}))])));
                        },
                    }
                    for event in frames {
                        if matches!(script, Script::Healthy) {
                            tokio::time::sleep(Duration::from_millis(160)).await;
                        }
                        yield Ok(Bytes::from(wire(vec![event])));
                    }
                };
                Response::builder().header("content-type", "application/vnd.amazon.eventstream")
                    .body(Body::from_stream(body)).unwrap()
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self {
            base,
            gate,
            dropped,
            task,
        }
    }
    fn adapter(&self) -> BedrockAdapter {
        BedrockAdapter::for_test_with_timeouts(&self.base, timeouts())
    }
    async fn stream(&self) -> EventStream {
        let mut req = request();
        req.stream = true;
        let ProviderOutput::Stream(output) = self.adapter().execute(&target(), req).await.unwrap()
        else {
            panic!("expected a stream")
        };
        output
    }
    async fn cancelled(&mut self) {
        tokio::time::timeout(Duration::from_secs(2), &mut self.dropped)
            .await
            .expect("dropping the receiver closes the upstream body")
            .unwrap();
    }
}
async fn first(stream: &mut EventStream) {
    assert!(matches!(
        stream.next().await,
        Some(Ok(ChatEvent::Delta { .. }))
    ));
}
async fn done(stream: &mut EventStream) {
    let events = stream.by_ref().collect::<Vec<_>>().await;
    assert!(
        matches!(events.last(), Some(Ok(ChatEvent::Done))),
        "errors: {:?}",
        events
            .iter()
            .filter_map(|event| event.as_ref().err())
            .collect::<Vec<_>>()
    );
    assert!(
        events.iter().all(Result::is_ok),
        "errors: {:?}",
        events
            .iter()
            .filter_map(|event| event.as_ref().err())
            .collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn healthy_stream_crosses_old_total_cap() {
    let mock = TimedMock::new(Script::Healthy).await;
    let start = Instant::now();
    done(&mut mock.stream().await).await;
    assert!(start.elapsed() > TOTAL);
}

#[tokio::test]
async fn downstream_pause_crosses_total_and_read_timers() {
    let mock = TimedMock::new(Script::Gated).await;
    let mut output = mock.stream().await;
    first(&mut output).await;
    // The server has not sent the tail yet, and the consumer does not poll.
    tokio::time::sleep(PAUSE).await;
    mock.gate.notify_one();
    done(&mut output).await;
}

#[tokio::test]
async fn first_downstream_poll_can_be_after_old_total_cap() {
    let mock = TimedMock::new(Script::Gated).await;
    let mut output = mock.stream().await;
    tokio::time::sleep(PAUSE).await;
    mock.gate.notify_one();
    done(&mut output).await;
}

#[tokio::test]
async fn active_upstream_stall_still_times_out_and_cancels() {
    let mut mock = TimedMock::new(Script::Stalled).await;
    let mut output = mock.stream().await;
    first(&mut output).await;
    let start = Instant::now();
    assert!(matches!(
        output.next().await,
        Some(Err(InferenceError::Timeout))
    ));
    assert!(start.elapsed() >= READ);
    drop(output);
    mock.cancelled().await;
}

#[tokio::test]
async fn non_stream_total_timeout_is_preserved() {
    // A complete JSON body makes progress within the read timeout, but never
    // finishes before the total cap. It must still be bounded by that cap.
    let app = Router::new().fallback(|| async {
        let body = async_stream::stream! {
            loop {
                yield Ok::<_, Infallible>(Bytes::from_static(b" "));
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        };
        Response::builder()
            .header("content-type", "application/json")
            .body(Body::from_stream(body))
            .unwrap()
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let start = Instant::now();
    let result = BedrockAdapter::for_test_with_timeouts(&base, timeouts())
        .execute(&target(), request())
        .await;
    task.abort();
    assert!(result.is_err());
    assert!(start.elapsed() >= TOTAL && start.elapsed() < Duration::from_secs(2));
}

#[tokio::test]
async fn downstream_pause_does_not_consume_active_receive_or_sdk_stall_budget() {
    let mock = TimedMock::new(Script::Gated).await;
    let adapter = BedrockAdapter::for_test_with_timeouts(
        &mock.base,
        transport::Timeouts {
            read: READ,
            total: Duration::from_secs(5),
        },
    );
    let mut req = request();
    req.stream = true;
    let ProviderOutput::Stream(mut output) = adapter.execute(&target(), req).await.unwrap() else {
        panic!("expected stream")
    };
    first(&mut output).await;
    tokio::time::sleep(PAUSE).await;
    mock.gate.notify_one();
    done(&mut output).await;
}

#[cfg(feature = "integration-tests")]
#[path = "timeout_db_tests.rs"]
mod db;

#[tokio::test]
async fn streaming_headers_still_have_an_active_read_timeout() {
    let app = Router::new().fallback(|| async {
        tokio::time::sleep(Duration::from_secs(5)).await;
        Response::builder().body(Body::empty()).unwrap()
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let mut req = request();
    req.stream = true;
    let start = Instant::now();
    let result = BedrockAdapter::for_test_with_timeouts(&base, timeouts())
        .execute(&target(), req)
        .await;
    task.abort();
    // Preserve main's pre-stream connector-timeout class.
    assert_eq!(result.err(), Some(InferenceError::UpstreamUnavailable));
    assert!(start.elapsed() >= READ && start.elapsed() < TOTAL);
}

#[tokio::test]
async fn non_stream_body_read_timeout_is_preserved() {
    let mut mock = TimedMock::new(Script::Stalled).await;
    let start = Instant::now();
    let result = mock.adapter().execute(&target(), request()).await;
    assert!(result.is_err());
    assert!(start.elapsed() >= READ && start.elapsed() < TOTAL);
    mock.cancelled().await;
}

#[tokio::test]
async fn streaming_http_error_body_keeps_its_active_read_bound() {
    let app = Router::new().fallback(|| async {
        let body =
            futures_util::stream::once(async { Ok::<_, Infallible>(Bytes::from_static(b"{")) })
                .chain(futures_util::stream::pending());
        Response::builder()
            .status(500)
            .header("content-type", "application/json")
            .body(Body::from_stream(body))
            .unwrap()
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let mut req = request();
    req.stream = true;
    let start = Instant::now();
    let result = BedrockAdapter::for_test_with_timeouts(&base, timeouts())
        .execute(&target(), req)
        .await;
    task.abort();
    assert!(result.is_err());
    assert!(start.elapsed() >= READ && start.elapsed() < TOTAL);
}

// A consumer may stop polling after Pending, not only after a yielded delta.
// The old reqwest body-read timer checked elapsed time before ready body data;
// the active recv timeout polls the receiver first, so queued healthy frames win.
#[tokio::test]
async fn ready_frames_after_unpolled_pending_receive_are_not_read_timeouts() {
    for old_read_timer in [true, false] {
        let mock = TimedMock::new(Script::Gated).await;
        let mut adapter = mock.adapter();
        if old_read_timer {
            // Re-create the old body-read policy, with a total cap beyond this
            // pause, to isolate the read timer from the fixed total timer.
            adapter.streaming_http = transport::client(
                transport::Timeouts {
                    read: READ,
                    total: Duration::from_secs(5),
                },
                false,
            )
            .unwrap();
        }
        let mut req = request();
        req.stream = true;
        let ProviderOutput::Stream(mut output) = adapter.execute(&target(), req).await.unwrap()
        else {
            panic!("expected stream")
        };
        first(&mut output).await;
        assert!(futures_util::poll!(output.next()).is_pending());
        mock.gate.notify_one();
        tokio::time::sleep(PAUSE).await;
        if old_read_timer {
            assert!(
                matches!(output.next().await, Some(Err(_))),
                "old read timer must reproduce the failure despite queued frames"
            );
        } else {
            done(&mut output).await;
        }
    }
}

async fn partial_resume(mock: &TimedMock, output: &mut EventStream) {
    first(output).await;
    assert!(futures_util::poll!(output.next()).is_pending());
    mock.gate.notify_one();
    tokio::time::sleep(PAUSE).await;
    // Model a backpressured transport needing another network turn after
    // the reader has consumed the queued prefix. Active read time stays < READ.
    tokio::join!(
        async {
            tokio::time::sleep(Duration::from_millis(50)).await;
            mock.gate.notify_one();
        },
        done(output)
    );
}

#[tokio::test]
async fn partial_frame_after_pending_reader_pause_gets_a_fresh_read_budget() {
    let mock = TimedMock::new(Script::Fragmented).await;
    let mut output = mock.stream().await;
    partial_resume(&mock, &mut output).await;
}
