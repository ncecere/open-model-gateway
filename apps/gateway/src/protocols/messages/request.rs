//! Explicit Claude Code field grammar. Nothing here is a passthrough payload.
use super::super::responses::{message, name, string, validate};
use super::*;
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    model: String,
    messages: Vec<WireMessage>,
    system: Option<Value>,
    max_tokens: u32,
    #[serde(default)]
    pub(super) stream: bool,
    temperature: Option<f64>,
    top_p: Option<f64>,
    top_k: Option<u32>,
    stop_sequences: Option<Vec<String>>,
    #[serde(default)]
    tools: Vec<Value>,
    tool_choice: Option<Value>,
    pub(super) metadata: Option<WireMetadata>,
    thinking: Option<Value>,
    output_config: Option<Value>,
    context_management: Option<Value>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct WireMetadata {
    pub(super) user_id: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireMessage {
    role: String,
    content: Value,
}
fn invalid(field: &'static str) -> InferenceError {
    InferenceError::InvalidField(field)
}
fn object(value: &Value, allowed: &[&str], field: &'static str) -> Result<()> {
    let values = value.as_object().ok_or_else(|| invalid(field))?;
    if let Some(unknown) = values.keys().find(|k| !allowed.contains(&k.as_str())) {
        return Err(crate::inference::error::FieldName::parse(unknown)
            .map_or_else(|| invalid(field), InferenceError::UnknownField));
    }
    Ok(())
}
fn cache(block: &Value, options: &mut MessagesOptions) -> Result<()> {
    if let Some(v) = block.get("cache_control") {
        object(v, &["type", "ttl"], "cache_control")?;
        if v["type"] != "ephemeral" || v.get("ttl").is_some_and(|v| v != "5m" && v != "1h") {
            return Err(invalid("cache_control"));
        }
        options.cache_controls.push(v.clone());
    }
    Ok(())
}
fn text_blocks(value: &Value, options: &mut MessagesOptions, images: bool) -> Result<Vec<String>> {
    if let Value::String(s) = value {
        return Ok(vec![s.clone()]);
    }
    let blocks = value.as_array().ok_or_else(|| invalid("content"))?;
    if blocks.len() > 256 {
        return Err(invalid("content"));
    }
    let mut text = vec![];
    for block in blocks {
        match block["type"].as_str() {
            Some("text") => {
                object(block, &["type", "text", "cache_control"], "text")?;
                cache(block, options)?;
                text.push(string(&block["text"])?);
            }
            Some("image") if images => image(block, options)?,
            _ => return Err(invalid("content.type")),
        }
    }
    Ok(text)
}
fn image(block: &Value, options: &mut MessagesOptions) -> Result<()> {
    object(block, &["type", "source", "cache_control"], "image")?;
    cache(block, options)?;
    let source = &block["source"];
    match source["type"].as_str() {
        Some("base64") => {
            object(source, &["type", "media_type", "data"], "image.source")?;
            if !matches!(
                source["media_type"].as_str(),
                Some("image/png" | "image/jpeg" | "image/gif" | "image/webp")
            ) || source["data"].as_str().is_none_or(str::is_empty)
            {
                return Err(invalid("image.source"));
            }
        }
        Some("url") => {
            object(source, &["type", "url"], "image.source")?;
            if source["url"].as_str().is_none_or(str::is_empty) {
                return Err(invalid("image.source"));
            }
        }
        _ => return Err(invalid("image.source")),
    }
    // Never fetch a client image URL. No generation adapter declares image input yet.
    options.images.push(block.clone());
    Ok(())
}
fn thinking(value: &Value, maximum: u32) -> Result<()> {
    object(value, &["type", "budget_tokens", "display"], "thinking")?;
    match value["type"].as_str() {
        Some("adaptive" | "disabled") if value.get("budget_tokens").is_none() => {}
        Some("enabled")
            if value["budget_tokens"]
                .as_u64()
                .is_some_and(|n| n >= 1024 && n < maximum as u64) => {}
        _ => return Err(invalid("thinking")),
    }
    if value
        .get("display")
        .is_some_and(|v| !matches!(v.as_str(), Some("summarized" | "omitted")))
    {
        return Err(invalid("thinking.display"));
    }
    Ok(())
}
fn output(value: &Value) -> Result<()> {
    object(value, &["effort", "format", "task_budget"], "output_config")?;
    if let Some(v) = value.get("task_budget") {
        object(
            v,
            &["type", "total", "remaining"],
            "output_config.task_budget",
        )?;
        if v["type"] != "tokens"
            || v["total"].as_u64().is_none_or(|n| n == 0)
            || v.get("remaining").is_some_and(|n| n.as_u64().is_none())
        {
            return Err(invalid("output_config.task_budget"));
        }
    }
    if value.get("effort").is_some_and(|v| {
        !matches!(
            v.as_str(),
            Some("low" | "medium" | "high" | "xhigh" | "max")
        )
    }) {
        return Err(invalid("output_config.effort"));
    }
    if let Some(format) = value.get("format") {
        object(format, &["type", "schema"], "output_config.format")?;
        if format["type"] != "json_schema" || !format["schema"].is_object() {
            return Err(invalid("output_config.format"));
        }
    }
    Ok(())
}
fn context(value: &Value) -> Result<()> {
    object(value, &["edits"], "context_management")?;
    let edits = value["edits"]
        .as_array()
        .ok_or_else(|| invalid("context_management.edits"))?;
    if edits.len() > 256 {
        return Err(invalid("context_management.edits"));
    }
    for edit in edits {
        object(
            edit,
            &[
                "type",
                "trigger",
                "keep",
                "clear_at_least",
                "clear_tool_inputs",
                "exclude_tools",
            ],
            "context_management.edits",
        )?;
        if !matches!(
            edit["type"].as_str(),
            Some("clear_tool_uses_20250919" | "clear_thinking_20251015")
        ) {
            return Err(invalid("context_management.edits.type"));
        }
        for field in ["trigger", "keep", "clear_at_least"] {
            if let Some(v) = edit.get(field) {
                if v == "all" && field == "keep" {
                    continue;
                }
                object(v, &["type", "value"], "context_management.edits")?;
                if !matches!(
                    v["type"].as_str(),
                    Some("input_tokens" | "tool_uses" | "thinking_turns")
                ) || v["value"].as_u64().is_none()
                {
                    return Err(invalid("context_management.edits"));
                }
            }
        }
        if edit
            .get("clear_tool_inputs")
            .is_some_and(|v| !v.is_boolean())
            || edit.get("exclude_tools").is_some_and(|v| {
                v.as_array()
                    .is_none_or(|a| a.iter().any(|v| !v.is_string()))
            })
        {
            return Err(invalid("context_management.edits"));
        }
    }
    Ok(())
}
impl Request {
    pub(super) fn normalize(self) -> Result<ChatRequest> {
        if self.messages.is_empty()
            || self.messages.len() > 4096
            || self.max_tokens == 0
            || self.temperature.is_some_and(|v| !(0.0..=1.0).contains(&v))
        {
            return Err(InferenceError::InvalidRequest);
        }
        if self.top_p.is_some_and(|v| !(0.0..=1.0).contains(&v)) {
            return Err(invalid("top_p"));
        }
        if self.top_k == Some(0) {
            return Err(invalid("top_k"));
        }
        if self
            .stop_sequences
            .as_ref()
            .is_some_and(|v| v.len() > 64 || v.iter().any(String::is_empty))
        {
            return Err(invalid("stop_sequences"));
        }
        if let Some(v) = &self.thinking {
            thinking(v, self.max_tokens)?;
        }
        if let Some(v) = &self.output_config {
            output(v)?;
        }
        if let Some(v) = &self.context_management {
            context(v)?;
        }
        let task_budget = self
            .output_config
            .as_ref()
            .and_then(|v| v.get("task_budget"))
            .cloned();
        let user_id = self.metadata.and_then(|m| m.user_id);
        if user_id.as_ref().is_some_and(|u| u.chars().count() > 256) {
            return Err(invalid("metadata.user_id"));
        }
        let mut options = MessagesOptions {
            top_p: self.top_p,
            top_k: self.top_k,
            stop_sequences: self.stop_sequences,
            user_id,
            system: self.system.clone(),
            thinking: self.thinking,
            output_config: self.output_config,
            context_management: self.context_management,
            task_budget,
            ..Default::default()
        };
        let mut messages = vec![];
        if let Some(system) = self.system {
            for text in text_blocks(&system, &mut options, false)? {
                messages.push(message(Role::System, text));
            }
        }
        for wire in self.messages {
            let role = match wire.role.as_str() {
                "user" => Role::User,
                "assistant" => Role::Assistant,
                _ => return Err(invalid("messages.role")),
            };
            if let Value::String(s) = wire.content {
                messages.push(message(role, s));
                continue;
            }
            let blocks = wire
                .content
                .as_array()
                .ok_or_else(|| invalid("messages.content"))?;
            if blocks.is_empty() || blocks.len() > 256 {
                return Err(invalid("messages.content"));
            }
            for block in blocks {
                match block["type"].as_str() {
                    Some("text") => {
                        object(block, &["type", "text", "cache_control"], "text")?;
                        cache(block, &mut options)?;
                        messages.push(message(role, string(&block["text"])?));
                    }
                    Some("image") => image(block, &mut options)?,
                    Some("thinking") if role == Role::Assistant => {
                        object(block, &["type", "thinking", "signature"], "thinking")?;
                        string(&block["thinking"])?;
                        string(&block["signature"])?;
                        options.history_thinking.push(block.clone());
                    }
                    Some("redacted_thinking") if role == Role::Assistant => {
                        object(block, &["type", "data"], "redacted_thinking")?;
                        string(&block["data"])?;
                        options.history_thinking.push(block.clone());
                    }
                    Some("tool_use") if role == Role::Assistant => {
                        object(
                            block,
                            &["type", "id", "name", "input", "cache_control"],
                            "tool_use",
                        )?;
                        cache(block, &mut options)?;
                        let id = string(&block["id"])?;
                        let n = string(&block["name"])?;
                        if id.is_empty() || !name(&n) || !block["input"].is_object() {
                            return Err(invalid("tool_use"));
                        }
                        messages.push(Message {
                            role,
                            content: None,
                            tool_call_id: None,
                            tool_calls: vec![ToolCall {
                                id,
                                name: n,
                                arguments: block["input"].to_string(),
                            }],
                        });
                    }
                    Some("tool_result") if role == Role::User => {
                        object(
                            block,
                            &[
                                "type",
                                "tool_use_id",
                                "content",
                                "is_error",
                                "cache_control",
                            ],
                            "tool_result",
                        )?;
                        cache(block, &mut options)?;
                        let id = string(&block["tool_use_id"])?;
                        if id.is_empty() {
                            return Err(invalid("tool_result.tool_use_id"));
                        }
                        if let Some(flag) = block.get("is_error")
                            && flag.as_bool().ok_or(InferenceError::InvalidRequest)?
                        {
                            return Err(invalid("tool_result.is_error"));
                        }
                        let content = block
                            .get("content")
                            .map(|v| text_blocks(v, &mut options, true).map(|v| v.concat()))
                            .transpose()?
                            .unwrap_or_default();
                        messages.push(Message {
                            role: Role::Tool,
                            content: Some(content),
                            tool_call_id: Some(id),
                            tool_calls: vec![],
                        });
                    }
                    Some(
                        "server_tool_use" | "web_search_tool_result" | "code_execution_tool_result",
                    ) => return Err(invalid("tools.type")),
                    _ => return Err(invalid("messages.content.type")),
                }
            }
        }
        // Image-only input has no text representation. Preserve its empty
        // canonical envelope only so the adapter can name the image refusal;
        // it must never dispatch. Do not invent input for thinking-only history.
        if messages.is_empty() && !options.images.is_empty() {
            messages.push(Message {
                role: Role::User,
                content: None,
                tool_calls: vec![],
                tool_call_id: None,
            });
        }
        let mut tools = vec![];
        for t in self.tools {
            if t.get("type").is_some_and(|v| v != "custom") {
                return Err(invalid("tools.type"));
            }
            object(
                &t,
                &[
                    "type",
                    "name",
                    "description",
                    "input_schema",
                    "strict",
                    "defer_loading",
                    "cache_control",
                ],
                "tools",
            )?;
            cache(&t, &mut options)?;
            if let Some(v) = t.get("defer_loading") {
                v.as_bool().ok_or_else(|| invalid("defer_loading"))?;
                options.defer_loading.push(v.clone());
            }
            tools.push(FunctionTool {
                name: string(&t["name"])?,
                description: t.get("description").map(string).transpose()?,
                parameters: t["input_schema"].clone(),
                strict: t
                    .get("strict")
                    .map(|v| v.as_bool().ok_or_else(|| invalid("strict")))
                    .transpose()?,
            });
        }
        let tool_choice = self
            .tool_choice
            .map(|v| {
                object(
                    &v,
                    &["type", "name", "disable_parallel_tool_use"],
                    "tool_choice",
                )?;
                if let Some(flag) = v.get("disable_parallel_tool_use") {
                    flag.as_bool()
                        .ok_or_else(|| invalid("tool_choice.disable_parallel_tool_use"))?;
                    // No shared parallel-tool control yet; refuse, never discard.
                    return Err(invalid("tool_choice.disable_parallel_tool_use"));
                }
                Ok(match v["type"].as_str() {
                    Some("auto") if v.get("name").is_none() => ToolChoice::Auto,
                    Some("none") if v.get("name").is_none() => ToolChoice::None,
                    Some("any") if v.get("name").is_none() => ToolChoice::Required,
                    Some("tool") => ToolChoice::Function(string(&v["name"])?),
                    _ => return Err(invalid("tool_choice")),
                })
            })
            .transpose()?;
        validate(ChatRequest {
            model: self.model,
            messages,
            tools,
            tool_choice,
            max_output_tokens: Some(self.max_tokens),
            temperature: self.temperature,
            stream: self.stream,
            messages_options: Some(Box::new(options)),
        })
    }
}
#[cfg(test)]
pub(super) fn parse_request(value: Value) -> Result<ChatRequest> {
    serde_json::from_value::<Request>(value)
        .map_err(|_| InferenceError::InvalidRequest)?
        .normalize()
}
