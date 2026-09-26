//! Shared adapter test contract. New adapters run this against their local mock upstream.
use super::ProviderAdapter;
use crate::inference::types::*;
use futures_util::StreamExt;

pub async fn assert_text_chat_contract(
    adapter: &dyn ProviderAdapter,
    target: &Deployment,
    mut request: ChatRequest,
) {
    request.stream = false;
    assert!(matches!(
        adapter.execute(target, request.clone()).await.unwrap(),
        ProviderOutput::Complete(_)
    ));
    request.stream = true;
    let ProviderOutput::Stream(mut output) = adapter.execute(target, request).await.unwrap() else {
        panic!("adapter did not return requested stream")
    };
    let mut finished = false;
    let mut done = false;
    let mut usage = false;
    while let Some(event) = output.next().await {
        assert!(!done, "events after Done");
        match event.unwrap() {
            ChatEvent::Delta { .. } => assert!(!finished, "delta after Finish"),
            ChatEvent::Finish(_) => {
                assert!(!finished, "duplicate Finish");
                finished = true;
            }
            ChatEvent::Usage(_) => {
                assert!(!usage, "duplicate Usage");
                usage = true;
            }
            ChatEvent::Done => {
                assert!(finished, "Done without Finish");
                done = true;
            }
        }
    }
    assert!(done, "EOF without Done");
}
