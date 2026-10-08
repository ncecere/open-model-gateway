//! Deadline cancellation independent of downstream polling; never produces network data in a task.
use super::{error::InferenceError, types::ChatEvent};
use futures_util::{Stream, task::AtomicWaker};
use std::{
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
};
use tokio::{task::JoinHandle, time::Instant};
/// Any boxed fallible stream; generation uses [`EventStream`], speech bytes.
pub(super) type Boxed<T> = Pin<Box<dyn Stream<Item = Result<T, InferenceError>> + Send>>;
struct State<T> {
    stream: Option<Boxed<T>>,
    expired: bool,
    error_emitted: bool,
}
struct Shared<T> {
    state: Mutex<State<T>>,
    waker: AtomicWaker,
    permit: Option<super::SharedPermit>,
}
pub(super) struct DeadlineStream<T: Send + 'static = ChatEvent> {
    shared: Arc<Shared<T>>,
    watchdog: JoinHandle<()>,
}
impl<T: Send + 'static> DeadlineStream<T> {
    pub fn new(stream: Boxed<T>, deadline: Instant, permit: Option<super::SharedPermit>) -> Self {
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                stream: Some(stream),
                expired: false,
                error_emitted: false,
            }),
            waker: AtomicWaker::new(),
            permit,
        });
        let weak = Arc::downgrade(&shared);
        let watchdog = tokio::spawn(async move {
            tokio::time::sleep_until(deadline).await;
            if let Some(shared) = weak.upgrade() {
                let transport = {
                    let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
                    state.expired = true;
                    state.stream.take()
                };
                drop(transport);
                if let Some(permit) = &shared.permit {
                    permit.lock().unwrap_or_else(|e| e.into_inner()).take();
                }
                shared.waker.wake();
            }
        });
        Self { shared, watchdog }
    }
}
impl<T: Send + 'static> Stream for DeadlineStream<T> {
    type Item = Result<T, InferenceError>;
    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.shared.waker.register(cx.waker());
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.expired && !state.error_emitted {
            state.error_emitted = true;
            return Poll::Ready(Some(Err(InferenceError::Timeout)));
        }
        match &mut state.stream {
            Some(stream) => stream.as_mut().poll_next(cx),
            None => Poll::Ready(None),
        }
    }
}
impl<T: Send + 'static> Drop for DeadlineStream<T> {
    fn drop(&mut self) {
        self.watchdog.abort();
        self.shared
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .stream
            .take();
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    struct Pending(Arc<AtomicBool>);
    impl Stream for Pending {
        type Item = Result<ChatEvent, InferenceError>;
        fn poll_next(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            Poll::Pending
        }
    }
    impl Drop for Pending {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }
    #[tokio::test]
    async fn deadline_drops_completely_unpolled_transport() {
        let dropped = Arc::new(AtomicBool::new(false));
        let stream = DeadlineStream::new(
            Box::pin(Pending(dropped.clone())),
            Instant::now() + std::time::Duration::from_millis(10),
            None,
        );
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while !dropped.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        drop(stream);
    }
    #[tokio::test]
    async fn drop_cancels_transport_synchronously() {
        let dropped = Arc::new(AtomicBool::new(false));
        let stream = DeadlineStream::new(
            Box::pin(Pending(dropped.clone())),
            Instant::now() + std::time::Duration::from_secs(60),
            None,
        );
        drop(stream);
        assert!(dropped.load(Ordering::SeqCst));
    }
}
