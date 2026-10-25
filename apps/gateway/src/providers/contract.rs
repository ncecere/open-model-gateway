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

/// Input-only token metering the mock reports (`None`: not reported).
fn assert_input_only(usage: &Usage, input_tokens: Option<u64>) {
    assert_eq!(usage.input_tokens, input_tokens, "input tokens");
    let billing = usage.billing.expect("billing categories");
    assert_eq!(billing.total_input_tokens, input_tokens, "total input");
    let meters = usage.meters.expect("non-token meters");
    assert_eq!(meters.requests, Some(1), "one upstream request");
    assert_eq!(
        (meters.output_images, meters.input_characters),
        (Some(0), Some(0)),
        "meters a text workload cannot produce are semantic zeros"
    );
}

/// Rerank: results valid for the request (and accepted by the workload),
/// input-only tokens with semantic output zero, one request, and search units
/// exactly as reported (unknown stays `None`, never zero).
pub async fn assert_rerank_contract(
    adapter: &dyn ProviderAdapter,
    target: &Deployment,
    request: RerankRequest,
    input_tokens: Option<u64>,
    search_units: Option<u64>,
) -> RerankResponse {
    use crate::inference::workload::Workload;
    assert!(adapter.supports_protocol(ApiProtocol::Rerank));
    let response = adapter
        .execute_rerank(target, request.clone())
        .await
        .unwrap_or_else(|e| panic!("rerank failed: {e:?}"));
    assert!(
        response.valid_for(&request),
        "results valid for the request"
    );
    assert!(
        request.valid_response(&response),
        "workload accepts response"
    );
    assert_eq!(response.usage.output_tokens, Some(0), "input-only workload");
    assert_input_only(&response.usage, input_tokens);
    assert_eq!(response.usage.meters.unwrap().search_units, search_units);
    response
}

/// System One: one answer per question, both TypeSafe counters as reported,
/// one request and no search units.
pub async fn assert_systemone_contract(
    adapter: &dyn ProviderAdapter,
    target: &Deployment,
    request: SystemoneRequest,
    input_tokens: u64,
    output_tokens: u64,
) -> SystemoneResponse {
    use crate::inference::workload::Workload;
    assert!(adapter.supports_protocol(ApiProtocol::Systemone));
    let response = adapter
        .execute_systemone(target, request.clone())
        .await
        .unwrap_or_else(|e| panic!("System One failed: {e:?}"));
    assert!(
        request.valid_response(&response),
        "workload accepts response"
    );
    assert_eq!(response.usage.output_tokens, Some(output_tokens));
    assert_input_only(&response.usage, Some(input_tokens));
    assert_eq!(response.usage.meters.unwrap().search_units, Some(0));
    response
}

/// Normalize the public omission/default forms before testing adapter wires.
/// Chat omits parameters; Messages requires input_schema but may omit successful
/// tool-result content and is_error. Every result must keep the actual call ID.
pub fn no_argument_tool_requests() -> Vec<ChatRequest> {
    use crate::protocols::{chat_completions, messages};
    use serde_json::json;
    let schema = json!({"type":"object","properties":{},"additionalProperties":false});
    let mut requests = vec![chat_completions::batch_request(json!({
        "model":"public", "messages":[{"role":"user","content":"ping"},
            {"role":"assistant","tool_calls":[{"id":"ping-call","type":"function","function":{"name":"ping","arguments":"{}"}}]},
            {"role":"tool","tool_call_id":"ping-call","content":""}],
        "tools":[{"type":"function","function":{"name":"ping"}}]
    })).unwrap()];
    for flag in [None, Some(false)] {
        let mut block = json!({"type":"tool_result","tool_use_id":"ping-call"});
        if let Some(flag) = flag {
            block["is_error"] = json!(flag);
        }
        requests.push(messages::batch_request(json!({
            "model":"public", "max_tokens":32,
            "messages":[{"role":"user","content":"ping"},
                {"role":"assistant","content":[{"type":"tool_use","id":"ping-call","name":"ping","input":{}}]},
                {"role":"user","content":[block]}],
            "tools":[{"name":"ping","input_schema":schema}]
        })).unwrap());
    }
    for request in &requests {
        assert_eq!(request.tools[0].parameters, schema);
        assert_eq!(request.tools[0].strict, None);
    }
    requests
}

/// A Claude Code turn with parallel calls, reversed results, and distinct
/// follow-up turns. Text blocks in one message must not create extra turns.
pub fn parallel_messages_request() -> ChatRequest {
    crate::protocols::messages::batch_request(serde_json::json!({
        "model":"public", "max_tokens":32,
        "messages":[
            {"role":"user","content":[{"type":"text","text":"hello"},{"type":"text","text":" there"}]},
            {"role":"assistant","content":[
                {"type":"text","text":"checking "},
                {"type":"tool_use","id":"first-call","name":"ping","input":{"value":1}},
                {"type":"text","text":"both"},
                {"type":"tool_use","id":"second-call","name":"pong","input":{"value":2}},
                {"type":"text","text":"!"}
            ]},
            {"role":"user","content":[
                {"type":"tool_result","tool_use_id":"second-call","content":"second result"},
                {"type":"tool_result","tool_use_id":"first-call","content":"first result"}
            ]},
            {"role":"assistant","content":"done"},
            {"role":"user","content":"continue"}
        ],
        "tools":[
            {"name":"ping","input_schema":{"type":"object","properties":{"value":{"type":"integer"}}}},
            {"name":"pong","input_schema":{"type":"object","properties":{"value":{"type":"integer"}}}}
        ]
    })).unwrap()
}

pub fn assert_parallel_chat_wire(body: &serde_json::Value) {
    use serde_json::json;
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(
        messages.len(),
        6,
        "original turns, with one canonical result per call"
    );
    assert_eq!(messages[0]["content"], "hello there");
    assert_eq!(messages[1]["role"], "assistant");
    assert_eq!(messages[1]["content"], "checking both!");
    let calls = messages[1]["tool_calls"].as_array().unwrap();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0]["id"], "first-call");
    assert_eq!(calls[0]["function"]["name"], "ping");
    assert_eq!(
        calls[0]["function"]["arguments"],
        json!({"value":1}).to_string()
    );
    assert_eq!(calls[1]["id"], "second-call");
    assert_eq!(calls[1]["function"]["name"], "pong");
    assert_eq!(
        calls[1]["function"]["arguments"],
        json!({"value":2}).to_string()
    );
    for (index, id, text) in [
        (2, "second-call", "second result"),
        (3, "first-call", "first result"),
    ] {
        assert_eq!(messages[index]["role"], "tool");
        assert_eq!(messages[index]["tool_call_id"], id);
        assert_eq!(messages[index]["content"], text);
    }
    assert_eq!(messages[4]["role"], "assistant");
    assert_eq!(messages[4]["content"], "done");
    assert_eq!(messages[5]["role"], "user");
    assert_eq!(messages[5]["content"], "continue");
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
            ChatEvent::Delta { .. } | ChatEvent::MessagesBlock(_) => {
                assert!(!finished, "delta after Finish")
            }
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
