//! Test-only wire sequence validation for generation protocols (#82).
use std::collections::BTreeSet;

use anyhow::{Context, Result, ensure};
use serde_json::Value;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Protocol {
    Chat { include_usage: bool },
    Responses,
    Messages,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Outcome {
    Completed,
    Incomplete,
    Errored,
    Truncated,
}
impl Outcome {
    pub(crate) fn positive(self) -> bool {
        self != Self::Truncated
    }
}
/// Validate framing and protocol order independently of payload schemas. Callers
/// must additionally validate each JSON payload against its artifact event ref.
/// A missing terminal (including a partially received final frame) never earns
/// positive coverage. Diagnostics describe kinds/positions, never wire values.
pub(crate) fn validate(protocol: Protocol, bytes: &[u8]) -> Result<Outcome> {
    let text = std::str::from_utf8(bytes).context("SSE is not UTF-8")?;
    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    let mut terminal = None;
    let mut count = 0;
    let mut blocks = BTreeSet::new();
    let mut used_blocks = BTreeSet::new();
    let mut message_delta = false;
    let mut remainder = text.as_str();
    while let Some((frame, rest)) = remainder.split_once("\n\n") {
        remainder = rest;
        let mut event = None;
        let mut data = Vec::new();
        for line in frame.lines() {
            if line.starts_with(':') || line.is_empty() {
                continue;
            }
            let (field, value) = line.split_once(':').unwrap_or((line, ""));
            let value = value.strip_prefix(' ').unwrap_or(value);
            match field {
                "event" => {
                    ensure!(event.replace(value).is_none(), "duplicate SSE event field");
                }
                "data" => data.push(value),
                // Standard SSE metadata does not alter the generation grammar.
                "id" | "retry" => {}
                _ => anyhow::bail!("unknown SSE field"),
            }
        }
        if data.is_empty() {
            continue;
        }
        ensure!(terminal.is_none(), "SSE data after terminal");
        let data = data.join("\n");
        if data == "[DONE]" {
            ensure!(matches!(protocol, Protocol::Chat { .. }), "unexpected DONE");
            ensure!(event.is_none() && count > 0, "DONE before Chat role chunk");
            terminal = Some(Outcome::Completed);
            continue;
        }
        let value: Value = serde_json::from_str(&data)
            .map_err(|e| anyhow::anyhow!("SSE data is not JSON ({:?})", e.classify()))?;
        ensure!(value.is_object(), "SSE data must be an object");
        match protocol {
            Protocol::Chat { include_usage } => {
                ensure!(event.is_none(), "Chat has an invented event type");
                if value.get("error").is_some() {
                    ensure!(count > 0, "Chat error before role chunk");
                    error(&value["error"], true)?;
                    terminal = Some(Outcome::Errored);
                } else {
                    let choices = value["choices"]
                        .as_array()
                        .context("Chat choices missing")?;
                    if count == 0 {
                        ensure!(
                            choices.len() == 1 && choices[0]["delta"]["role"] == "assistant",
                            "Chat first chunk is not assistant role"
                        );
                    }
                    ensure!(
                        include_usage || value.get("usage").is_none(),
                        "unrequested Chat usage"
                    );
                    ensure!(
                        !choices.is_empty() || value["usage"].is_object(),
                        "empty Chat choices without usage"
                    );
                }
            }
            Protocol::Responses => {
                let kind = event.context("Responses event type missing")?;
                ensure!(value["type"] == kind, "Responses event/data type mismatch");
                ensure!(
                    value["sequence_number"].as_u64() == Some(count),
                    "Responses sequence gap"
                );
                if count < 2 {
                    ensure!(
                        kind == if count == 0 {
                            "response.created"
                        } else {
                            "response.in_progress"
                        },
                        "Responses opening order"
                    );
                } else {
                    match kind {
                        "response.completed" => terminal = Some(Outcome::Completed),
                        "response.incomplete" => terminal = Some(Outcome::Incomplete),
                        "error" => {
                            error(&value, true)?;
                            terminal = Some(Outcome::Errored);
                        }
                        "response.output_item.added"
                        | "response.content_part.added"
                        | "response.output_text.delta"
                        | "response.output_text.done"
                        | "response.content_part.done"
                        | "response.function_call_arguments.delta"
                        | "response.function_call_arguments.done"
                        | "response.output_item.done" => {}
                        _ => anyhow::bail!("unknown or misplaced Responses event"),
                    }
                }
            }
            Protocol::Messages => {
                let kind = event.context("Messages event type missing")?;
                ensure!(value["type"] == kind, "Messages event/data type mismatch");
                if count == 0 {
                    ensure!(
                        kind == "message_start",
                        "Messages must start with message_start"
                    );
                } else {
                    match kind {
                        "content_block_start" | "content_block_delta" | "content_block_stop" => {
                            ensure!(!message_delta, "Messages block after message_delta");
                            let index = value["index"]
                                .as_u64()
                                .context("Messages block index missing")?;
                            match kind {
                                "content_block_start" => {
                                    ensure!(
                                        used_blocks.insert(index),
                                        "Messages reused block index"
                                    );
                                    blocks.insert(index);
                                }
                                "content_block_delta" => ensure!(
                                    blocks.contains(&index),
                                    "Messages delta without open block"
                                ),
                                _ => ensure!(
                                    blocks.remove(&index),
                                    "Messages stop without open block"
                                ),
                            }
                        }
                        "message_delta" => {
                            ensure!(
                                blocks.is_empty() && !message_delta,
                                "Messages delta with unclosed blocks or duplicate delta"
                            );
                            message_delta = true;
                        }
                        "message_stop" => {
                            ensure!(
                                message_delta && blocks.is_empty(),
                                "Messages stop before delta or block stop"
                            );
                            terminal = Some(Outcome::Completed);
                        }
                        "error" => {
                            error(&value["error"], false)?;
                            terminal = Some(Outcome::Errored);
                        }
                        _ => anyhow::bail!("unknown or misplaced Messages event"),
                    }
                }
            }
        }
        count += 1;
    }
    if !remainder.is_empty() {
        return Ok(Outcome::Truncated);
    }
    Ok(terminal.unwrap_or(Outcome::Truncated))
}

fn error(value: &Value, openai: bool) -> Result<()> {
    ensure!(value["message"].is_string(), "SSE error message missing");
    if openai {
        ensure!(
            value["code"].is_string() && value.get("param") == Some(&Value::Null),
            "OpenAI SSE error shape"
        );
    } else {
        ensure!(
            value["type"].is_string() && value.get("code").is_none(),
            "Anthropic SSE error shape"
        );
    }
    Ok(())
}
