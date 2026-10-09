//! Upstream scheduling hint of gateway-run batch lines (0022,
//! docs/batches.md#scheduling-on-self-hosted-models).
//!
//! A route whose server runs vLLM with `--scheduling-policy priority` may be
//! configured to receive `priority: <n>` on batch lines (vLLM: lower values
//! are handled earlier; requests without the field use 0, so batch lines with
//! a positive value never get ahead of live traffic). The batch runner scopes
//! the value to the one route a line is pinned to; adapters that support the
//! field read it for that exact route only. Interactive requests never carry
//! it, and routes without the setting never receive the field.
use std::future::Future;

use uuid::Uuid;

tokio::task_local! {
    static PRIORITY: (Uuid, i32);
}

/// Run `f` with the batch priority hint for `deployment`.
pub async fn with_priority<F: Future>(deployment: Uuid, priority: i32, f: F) -> F::Output {
    PRIORITY.scope((deployment, priority), f).await
}

/// The priority hint for `deployment` in the current task, if any.
pub fn priority_for(deployment: Uuid) -> Option<i32> {
    PRIORITY
        .try_with(|(d, p)| (*d == deployment).then_some(*p))
        .ok()
        .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_hint_is_scoped_to_one_route() {
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
        assert_eq!(priority_for(a), None);
        with_priority(a, 10, async {
            assert_eq!(priority_for(a), Some(10));
            assert_eq!(priority_for(b), None);
        })
        .await;
        assert_eq!(priority_for(a), None);
    }
}
