//! Usage observed in an upstream body that failed structural validation.
//!
//! The attempt still fails (and keeps its hold), but a valid usage object the
//! provider returned is independent metering evidence and must not be lost.
//! Adapters report it through [`preserve`]; the engine collects it with
//! [`capture`] around the adapter call and each upstream stream poll. Outside a
//! capture scope (for example in unit tests) reporting is a no-op.
use super::{error::InferenceError, types::Usage};
use std::{cell::Cell, future::Future};

tokio::task_local! {
    static INVALID_BODY_USAGE: Cell<Option<Usage>>;
}

/// Run `future`, returning its output and any usage reported by an adapter
/// whose upstream body failed validation during this poll scope.
pub(crate) async fn capture<F: Future>(future: F) -> (F::Output, Option<Usage>) {
    INVALID_BODY_USAGE
        .scope(Cell::new(None), async move {
            let output = future.await;
            (output, INVALID_BODY_USAGE.with(Cell::take))
        })
        .await
}

/// Pass `result` through unchanged. If it failed and `usage` (the body's own
/// usage object, parsed with the normal strict metering rules) is valid and
/// present, report it as observed evidence for the failed attempt.
pub(crate) fn preserve<T>(
    result: Result<T, InferenceError>,
    usage: impl FnOnce() -> Option<Result<Usage, InferenceError>>,
) -> Result<T, InferenceError> {
    if result.is_err()
        && let Some(Ok(observed)) = usage()
        && observed != Usage::default()
    {
        let _ = INVALID_BODY_USAGE.try_with(|cell| cell.set(Some(observed)));
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn reports_only_failed_results_inside_scope() {
        let usage = Usage {
            input_tokens: Some(3),
            output_tokens: Some(2),
            billing: None,
            ..Default::default()
        };
        let (out, seen) = capture(async {
            preserve::<()>(Err(InferenceError::InvalidUpstream), || Some(Ok(usage)))
        })
        .await;
        assert_eq!(out, Err(InferenceError::InvalidUpstream));
        assert_eq!(seen, Some(usage));
        let (_, seen) = capture(async { preserve(Ok(()), || Some(Ok(usage))) }).await;
        assert_eq!(seen, None);
        let (_, seen) = capture(async {
            preserve::<()>(Err(InferenceError::InvalidUpstream), || {
                Some(Err(InferenceError::InvalidUpstream))
            })
        })
        .await;
        assert_eq!(seen, None);
        // Outside a scope reporting is a harmless no-op.
        assert!(preserve::<()>(Err(InferenceError::InvalidUpstream), || Some(Ok(usage))).is_err());
    }
}
