//! Reasoning models on local servers. Fixtures are the exact field shapes a
//! live SGLang server (Qwen with a reasoning parser) returned on 2026-10-09:
//! `reasoning_content` beside `content`, an integer `matched_stop` on a stop
//! finish, and top-level `usage.reasoning_tokens`.
use super::*;
use axum::{
    Router,
    body::{Body, Bytes},
    http::Response,
};
use futures_util::StreamExt;
use serde_json::{Value, json};

struct NoSecrets;
impl SecretResolver for NoSecrets {
    fn resolve(&self, _: &str) -> Result<super::super::secrets::Secret> {
        Err(InferenceError::Configuration)
    }
}

async fn serve(complete: Value, frames: Vec<Value>) -> (String, tokio::task::JoinHandle<()>) {
    let app = Router::new().fallback(move |bytes: Bytes| {
        let (complete, frames) = (complete.clone(), frames.clone());
        async move {
            let request: Value = serde_json::from_slice(&bytes).unwrap();
            if request["stream"] == true {
                let mut body: String = frames.iter().map(|f| format!("data: {f}\n\n")).collect();
                body.push_str("data: [DONE]\n\n");
                Response::builder()
                    .header("content-type", "text/event-stream")
                    .body(Body::from(body))
                    .unwrap()
            } else {
                Response::builder()
                    .header("content-type", "application/json")
                    .body(Body::from(complete.to_string()))
                    .unwrap()
            }
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!(
        "http://127.0.0.1:{}/v1",
        listener.local_addr().unwrap().port()
    );
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (base, task)
}

fn target(profile: Profile, base: &str) -> Deployment {
    Deployment {
        id: uuid::Uuid::new_v4(),
        provider: profile.id().into(),
        upstream_model: "qwen".into(),
        credential_ref: "none".into(),
        endpoint: Some(base.into()),
        region: None,
        supported_protocols: vec!["chat_completions".into()],
    }
}

fn request(stream: bool) -> ChatRequest {
    ChatRequest {
        model: "public".into(),
        messages: vec![Message {
            role: Role::User,
            content: Some("hello".into()),
            tool_calls: vec![],
            tool_call_id: None,
        }],
        tools: vec![],
        tool_choice: None,
        temperature: None,
        max_output_tokens: Some(1024),
        stream,
    }
}

fn sglang_complete(matched_stop: Value, reasoning: Value) -> Value {
    json!({"id":"x","object":"chat.completion","created":1,"model":"qwen3.8-27b",
        "choices":[{"index":0,"message":{"role":"assistant","content":"OK","reasoning_content":reasoning,"tool_calls":null},
            "logprobs":null,"finish_reason":"stop","matched_stop":matched_stop}],
        "usage":{"prompt_tokens":59,"total_tokens":83,"completion_tokens":24,"prompt_tokens_details":null,"reasoning_tokens":21},
        "metadata":{"weight_version":"default"}})
}

fn sglang_frames() -> Vec<Value> {
    vec![
        json!({"id":"x","object":"chat.completion.chunk","created":1,"model":"qwen3.8-27b","choices":[{"index":0,"delta":{"role":"assistant","content":"","reasoning_content":null},"logprobs":null,"finish_reason":null,"matched_stop":null}],"usage":null}),
        json!({"id":"x","object":"chat.completion.chunk","created":1,"model":"qwen3.8-27b","choices":[{"index":0,"delta":{"role":null,"content":null,"reasoning_content":"The user wants"},"logprobs":null,"finish_reason":null,"matched_stop":null}],"usage":null}),
        json!({"id":"x","object":"chat.completion.chunk","created":1,"model":"qwen3.8-27b","choices":[{"index":0,"delta":{"role":null,"content":"OK","reasoning_content":null},"logprobs":null,"finish_reason":null,"matched_stop":null}],"usage":null}),
        json!({"id":"x","object":"chat.completion.chunk","created":1,"model":"qwen3.8-27b","choices":[{"index":0,"delta":{"role":null,"content":null,"reasoning_content":null},"logprobs":null,"finish_reason":"stop","matched_stop":248046}],"usage":null}),
        json!({"id":"x","object":"chat.completion.chunk","created":1,"model":"qwen3.8-27b","choices":[],"usage":{"prompt_tokens":59,"total_tokens":99,"completion_tokens":40,"prompt_tokens_details":null,"reasoning_tokens":37}}),
    ]
}

#[tokio::test]
async fn sglang_reasoning_trace_is_dropped_and_its_tokens_stay_in_output() {
    let (base, task) = serve(
        sglang_complete(json!(248046), json!("The user wants OK.")),
        sglang_frames(),
    )
    .await;
    for profile in [Profile::Sglang, Profile::Vllm, Profile::OpenAiCompatible] {
        let adapter = LocalAdapter::new(
            profile,
            Arc::new(NoSecrets),
            Arc::new(ApprovedEndpoints::for_test(&base)),
        );
        let target = target(profile, &base);
        let ProviderOutput::Complete(response) =
            adapter.execute(&target, request(false)).await.unwrap()
        else {
            panic!("expected a complete response")
        };
        assert_eq!(response.content.as_deref(), Some("OK"));
        assert_eq!(response.finish_reason, FinishReason::Stop);
        assert_eq!(response.usage.output_tokens, Some(24));
        assert_eq!(response.usage.reasoning_tokens, Some(21));

        let ProviderOutput::Stream(stream) = adapter.execute(&target, request(true)).await.unwrap()
        else {
            panic!("expected a stream")
        };
        let events: Vec<_> = stream.collect().await;
        let mut text = String::new();
        let mut usage = None;
        for event in &events {
            match event.as_ref().expect("stream must decode") {
                ChatEvent::Delta { text: Some(t), .. } => text.push_str(t),
                ChatEvent::Usage(u) => usage = Some(*u),
                _ => {}
            }
        }
        // Only `content` reaches the client; the trace never does.
        assert_eq!(text, "OK");
        assert!(matches!(events.last(), Some(Ok(ChatEvent::Done))));
        let usage = usage.unwrap();
        assert_eq!(usage.output_tokens, Some(40));
        assert_eq!(usage.reasoning_tokens, Some(37));
    }
    task.abort();
}

#[tokio::test]
async fn local_extras_keep_their_scalar_shapes() {
    for (matched_stop, reasoning) in [
        (json!({"token": 1}), json!("trace")),
        (json!([1]), json!("trace")),
        (json!(1), json!({"text": "trace"})),
        (json!(1), json!(7)),
    ] {
        let (base, task) = serve(sglang_complete(matched_stop, reasoning), vec![]).await;
        let adapter = LocalAdapter::new(
            Profile::Sglang,
            Arc::new(NoSecrets),
            Arc::new(ApprovedEndpoints::for_test(&base)),
        );
        let result = adapter
            .execute(&target(Profile::Sglang, &base), request(false))
            .await;
        assert!(matches!(result, Err(InferenceError::InvalidUpstream)));
        task.abort();
    }
}

#[test]
fn cloud_openai_decoding_still_rejects_reasoning_fields() {
    let value = sglang_complete(json!(248046), json!("trace"));
    assert!(super::super::openai::decode_complete(&value).is_err());
    let mut value = sglang_complete(Value::Null, Value::Null);
    assert!(super::super::openai::decode_complete(&value).is_ok());
    value["choices"][0]["message"]["reasoning_content"] = json!("trace");
    assert!(super::super::openai::decode_complete(&value).is_err());
}
