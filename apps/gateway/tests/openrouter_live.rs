//! Opt-in, paid-capable OpenRouter acceptance through the real adapter code.
//! Never runs in CI: it is `#[ignore]` and additionally requires
//! `OPENROUTER_LIVE=1` plus the server-held key in `OPEN_ROUTER`. It prints
//! only outcome and usage counters (never the key, prompts or outputs).
//! Expected spend: well under $0.001 (one 16-token chat, free embeddings and
//! rerank models, one tiny System One question).
use std::{collections::BTreeMap, sync::Arc};

use open_model_gateway::{
    inference::types::*,
    providers::{
        ProviderAdapter,
        openrouter::{DataCollection, OpenRouterAdapter, OpenRouterConfig},
        secrets::EnvSecrets,
    },
};
use serde_json::json;

fn target(model: &str) -> Deployment {
    Deployment {
        id: uuid::Uuid::new_v4(),
        provider: "openrouter".into(),
        upstream_model: model.into(),
        credential_ref: "env:OPEN_ROUTER".into(),
        endpoint: None,
        region: None,
        supported_protocols: vec![],
    }
}
fn report(name: &str, usage: Usage) {
    eprintln!(
        "LIVE {name}: ok input_tokens={:?} output_tokens={:?} total_input={:?} cache_read={:?} cache_write={:?} search_units={:?} requests={:?} provider_cost_microusd={:?}",
        usage.input_tokens,
        usage.output_tokens,
        usage.billing.and_then(|b| b.total_input_tokens),
        usage.billing.and_then(|b| b.cache_read_input_tokens),
        usage.billing.and_then(|b| b.cache_write_input_tokens),
        usage.meters.and_then(|m| m.search_units),
        usage.meters.and_then(|m| m.requests),
        usage.provider_cost_microusd,
    );
}

#[tokio::test]
#[ignore = "live OpenRouter request: set OPENROUTER_LIVE=1 and OPEN_ROUTER"]
async fn openrouter_live_chat_embeddings_rerank_systemone() {
    assert_eq!(
        std::env::var("OPENROUTER_LIVE").as_deref(),
        Ok("1"),
        "explicit opt-in required"
    );
    assert!(
        std::env::var_os("OPEN_ROUTER").is_some(),
        "OPEN_ROUTER unset"
    );
    let adapter = OpenRouterAdapter::new(
        Arc::new(EnvSecrets::new(["OPEN_ROUTER".to_owned()])),
        OpenRouterConfig::default(),
    )
    .unwrap();
    // OpenRouter `:free` endpoints may train on prompts, so the default
    // `data_collection:"deny"` leaves no endpoint (404 → upstream_rejected).
    // Only these free-model calls opt into `allow`, with trivial inputs.
    let free = OpenRouterAdapter::new(
        Arc::new(EnvSecrets::new(["OPEN_ROUTER".to_owned()])),
        OpenRouterConfig::new(None, None, DataCollection::Allow).unwrap(),
    )
    .unwrap();
    let mut failures = Vec::new();
    // Optional comma-separated subset, e.g. OPENROUTER_LIVE_ONLY=rerank,embeddings.
    let only = std::env::var("OPENROUTER_LIVE_ONLY").ok();
    let run = |name: &str| {
        only.as_deref()
            .is_none_or(|o| o.split(',').any(|s| s == name))
    };

    let chat = ChatRequest {
        model: "live/glm".into(),
        messages: vec![Message {
            role: Role::User,
            content: Some("Say hi".into()),
            tool_calls: vec![],
            tool_call_id: None,
        }],
        tools: vec![],
        tool_choice: None,
        temperature: None,
        max_output_tokens: Some(16),
        stream: false,
    };
    if run("chat") {
        match adapter.execute(&target("z-ai/glm-5.3-flash"), chat).await {
            Ok(ProviderOutput::Complete(r)) => {
                eprintln!("LIVE chat finish={:?}", r.finish_reason);
                assert!(r.usage.input_tokens.is_some() && r.usage.output_tokens.is_some());
                report("chat z-ai/glm-5.3-flash", r.usage);
            }
            Ok(ProviderOutput::Stream(_)) => failures.push("chat: unexpected stream".to_owned()),
            Err(e) => failures.push(format!("chat: {}", e.code())),
        }
    }

    let embed = EmbeddingRequest {
        model: "live/embed".into(),
        input: vec!["hello".into()],
        dimensions: None,
    };
    if run("embeddings") {
        let denied = adapter
            .execute_embeddings(&target("nvidia/nemotron-3-embed-1b:free"), embed.clone())
            .await
            .err();
        eprintln!(
            "LIVE embeddings with data_collection=deny: {:?}",
            denied.map(|e| e.code())
        );
        match free
            .execute_embeddings(&target("nvidia/nemotron-3-embed-1b:free"), embed)
            .await
        {
            Ok(r) => {
                eprintln!(
                    "LIVE embeddings vectors={} width={}",
                    r.embeddings.len(),
                    r.embeddings[0].len()
                );
                assert_eq!(r.embeddings[0].len(), 2048);
                report("embeddings nvidia/nemotron-3-embed-1b:free", r.usage);
            }
            Err(e) => failures.push(format!("embeddings: {}", e.code())),
        }
    }

    let rerank = RerankRequest {
        model: "live/rerank".into(),
        query: "cat".into(),
        documents: vec!["kitten".into(), "airplane".into()],
        top_n: None,
    };
    if run("rerank") {
        match free
            .execute_rerank(
                &target("nvidia/llama-nemotron-rerank-vl-1b-v2:free"),
                rerank,
            )
            .await
        {
            Ok(r) => {
                eprintln!("LIVE rerank results={}", r.results.len());
                report("rerank nvidia/llama-nemotron-rerank-vl-1b-v2:free", r.usage);
            }
            Err(e) => failures.push(format!("rerank: {}", e.code())),
        }
    }

    let systemone = SystemoneRequest {
        model: "live/clef".into(),
        state: json!("hello there"),
        questions: BTreeMap::from([(
            "is_greeting".to_owned(),
            Question {
                kind: QuestionKind::Noul,
                instructions: json!("Is the text a greeting?"),
                criteria: None,
            },
        )]),
    };
    if run("systemone") {
        match adapter
            .execute_systemone(&target("cloudflare/clef-flash"), systemone)
            .await
        {
            Ok(r) => {
                eprintln!("LIVE systemone answers={}", r.answers.len());
                report("systemone cloudflare/clef-flash", r.usage);
            }
            Err(e) => failures.push(format!("systemone: {}", e.code())),
        }
    }
    assert!(failures.is_empty(), "{failures:?}");
}
