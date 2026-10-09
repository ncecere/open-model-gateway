//! Shared adapter test contract. New adapters run this against their local mock upstream.
use super::ProviderAdapter;
use crate::inference::types::*;
use futures_util::StreamExt;

/// Logs telemetry the mock upstream reports identically for its complete and
/// streamed replies. `None` means the mock reports nothing, so the adapter
/// must leave it unknown (never a fabricated zero or the configured model).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Telemetry<'a> {
    pub reported_model: Option<&'a str>,
    pub reasoning_tokens: Option<u64>,
}
impl Telemetry<'_> {
    pub const UNKNOWN: Telemetry<'static> = Telemetry {
        reported_model: None,
        reasoning_tokens: None,
    };
}
fn observed(usage: &Usage) -> Telemetry<'_> {
    Telemetry {
        reported_model: usage.reported_model.as_ref().map(ReportedModel::as_str),
        reasoning_tokens: usage.reasoning_tokens,
    }
}

pub async fn assert_text_chat_contract(
    adapter: &dyn ProviderAdapter,
    target: &Deployment,
    mut request: ChatRequest,
    expected: Telemetry<'_>,
) {
    request.stream = false;
    let ProviderOutput::Complete(response) =
        adapter.execute(target, request.clone()).await.unwrap()
    else {
        panic!("adapter did not return requested complete response")
    };
    assert_eq!(
        observed(&response.usage),
        expected,
        "complete response telemetry"
    );
    request.stream = true;
    let ProviderOutput::Stream(mut output) = adapter.execute(target, request).await.unwrap() else {
        panic!("adapter did not return requested stream")
    };
    let mut finished = false;
    let mut done = false;
    let mut usage = None;
    while let Some(event) = output.next().await {
        assert!(!done, "events after Done");
        match event.unwrap() {
            ChatEvent::Delta { .. } => assert!(!finished, "delta after Finish"),
            ChatEvent::Finish(_) => {
                assert!(!finished, "duplicate Finish");
                finished = true;
            }
            ChatEvent::Usage(observed) => {
                assert!(usage.is_none(), "duplicate Usage");
                usage = Some(observed);
            }
            ChatEvent::Done => {
                assert!(finished, "Done without Finish");
                done = true;
            }
        }
    }
    assert!(done, "EOF without Done");
    assert_eq!(
        usage.as_ref().map_or(Telemetry::UNKNOWN, observed),
        expected,
        "streamed telemetry"
    );
}
