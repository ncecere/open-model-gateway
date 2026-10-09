//! Bedrock Converse adapter. AWS owns endpoint resolution, signing and wire decoding.
//! Prompt-bearing and credential-bearing state deliberately does not implement Debug.
use std::{
    collections::{BTreeMap, BTreeSet},
    time::Duration,
};

use async_trait::async_trait;
use aws_sdk_bedrockruntime::{
    Client,
    config::{BehaviorVersion, Region, retry::RetryConfig, timeout::TimeoutConfig},
    error::{ProvideErrorMetadata, SdkError},
    types as aws,
};
use aws_smithy_types::{Document, Number};
use serde_json::Value;
use tokio::sync::Mutex;

use super::ProviderAdapter;
use crate::inference::{error::InferenceError, types::*};

#[path = "bedrock/auth.rs"]
pub(crate) mod auth;
#[path = "bedrock/stream.rs"]
mod stream;
#[cfg(test)]
#[path = "bedrock/tests.rs"]
mod tests;
#[path = "bedrock/transport.rs"]
mod transport;
#[path = "bedrock/wire.rs"]
mod wire;

type Result<T> = std::result::Result<T, InferenceError>;
const OUTPUT_LIMIT: usize = 4 * 1024 * 1024;
const MAX_BLOCKS: usize = 256;
const MAX_TOOLS: usize = 128;

pub struct BedrockAdapter {
    http: aws_smithy_runtime_api::client::http::SharedHttpClient,
    clients: Mutex<BTreeMap<String, Client>>,
    /// Profile and endpoint allowlists, read from the environment at startup.
    policy: auth::Policy,
    #[cfg(test)]
    test_client: Option<Client>,
}

impl BedrockAdapter {
    pub fn new() -> anyhow::Result<Self> {
        Ok(Self {
            http: transport::client()?,
            clients: Mutex::new(BTreeMap::new()),
            policy: auth::Policy::from_env(),
            #[cfg(test)]
            test_client: None,
        })
    }

    async fn client(&self, plan: &auth::Plan) -> Client {
        #[cfg(test)]
        if let Some(client) = &self.test_client {
            return client.clone();
        }
        let key = plan.key();
        let mut clients = self.clients.lock().await;
        if let Some(client) = clients.get(&key) {
            return client.clone();
        }
        let credentials = auth::credentials(&plan.auth, &plan.region).await;
        let client = Client::from_conf(self.service_config(plan, credentials));
        // Bound cache cardinality even if catalog configuration changes repeatedly. Evicting
        // keeps a reused client (and its credential cache) for the current configuration.
        if clients.len() >= 64 {
            clients.pop_first();
        }
        clients.insert(key, client.clone());
        client
    }

    /// Built directly rather than copying a shared SDK configuration: environment/profile
    /// endpoint overrides (including AWS_ENDPOINT_URL_BEDROCK_RUNTIME) cannot be inherited.
    /// Only an allowlisted connection endpoint replaces the regional endpoint.
    fn service_config(
        &self,
        plan: &auth::Plan,
        credentials: aws_sdk_bedrockruntime::config::SharedCredentialsProvider,
    ) -> aws_sdk_bedrockruntime::Config {
        let mut builder = aws_sdk_bedrockruntime::Config::builder();
        builder.set_endpoint_url(plan.endpoint.clone());
        builder
            .behavior_version(BehaviorVersion::latest())
            .region(Region::new(plan.region.clone()))
            .credentials_provider(credentials)
            .http_client(self.http.clone())
            .retry_config(RetryConfig::standard().with_max_attempts(1))
            .timeout_config(
                TimeoutConfig::builder()
                    .connect_timeout(Duration::from_secs(10))
                    .read_timeout(Duration::from_secs(60))
                    .operation_timeout(Duration::from_secs(300))
                    .build(),
            )
            .build()
    }
}

#[async_trait]
impl ProviderAdapter for BedrockAdapter {
    fn id(&self) -> &'static str {
        "bedrock"
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            text_chat: true,
            streaming: true,
            tools: true,
        }
    }
    fn supports_protocol(&self, protocol: ApiProtocol) -> bool {
        matches!(
            protocol,
            ApiProtocol::ChatCompletions | ApiProtocol::Messages
        )
    }
    async fn execute(&self, target: &Deployment, request: ChatRequest) -> Result<ProviderOutput> {
        let plan = auth::Plan::new(target, &self.policy).ok_or(InferenceError::Configuration)?;
        let input = encode(&request)?; // Validate before any credential discovery/network work.
        let client = self.client(&plan).await;
        if request.stream {
            let output = client
                .converse_stream()
                .model_id(&target.upstream_model)
                .set_messages(Some(input.messages))
                .set_system(input.system)
                .inference_config(input.inference)
                .set_tool_config(input.tools)
                .send()
                .await
                .map_err(sdk_error)?;
            Ok(ProviderOutput::Stream(stream::decode(output.stream)))
        } else {
            let output = client
                .converse()
                .model_id(&target.upstream_model)
                .set_messages(Some(input.messages))
                .set_system(input.system)
                .inference_config(input.inference)
                .set_tool_config(input.tools)
                .send()
                .await
                .map_err(sdk_error)?;
            Ok(ProviderOutput::Complete(decode_complete(output)?))
        }
    }
}

struct Input {
    messages: Vec<aws::Message>,
    system: Option<Vec<aws::SystemContentBlock>>,
    inference: aws::InferenceConfiguration,
    tools: Option<aws::ToolConfiguration>,
}

fn encode(request: &ChatRequest) -> Result<Input> {
    let invalid = || InferenceError::InvalidRequest;
    if request.messages.is_empty()
        || request.messages.len() > 1024
        || request.tools.len() > MAX_TOOLS
        || request
            .temperature
            .is_some_and(|t| !t.is_finite() || !(0.0..=1.0).contains(&t))
        || request
            .max_output_tokens
            .is_some_and(|n| n == 0 || n > i32::MAX as u32)
    {
        return Err(invalid());
    }
    let mut names = BTreeSet::new();
    let mut tools = Vec::new();
    let mut size = 0usize;
    for tool in &request.tools {
        // Bedrock's strict support is model-specific; never silently weaken a strict schema.
        if tool.strict == Some(true) {
            return Err(InferenceError::Unsupported);
        }
        if !identifier(&tool.name)
            || !names.insert(tool.name.clone())
            || !tool.parameters.is_object()
            || tool.parameters.get("type").and_then(Value::as_str) != Some("object")
        {
            return Err(invalid());
        }
        size += tool.name.len()
            + tool.description.as_ref().map_or(0, String::len)
            + serde_json::to_vec(&tool.parameters)
                .map_err(|_| invalid())?
                .len();
        if size > OUTPUT_LIMIT {
            return Err(invalid());
        }
        tools.push(aws::Tool::ToolSpec(
            aws::ToolSpecification::builder()
                .name(&tool.name)
                .set_description(tool.description.clone())
                .input_schema(aws::ToolInputSchema::Json(document(&tool.parameters)?))
                .build()
                .map_err(|_| invalid())?,
        ));
    }
    let choice = match &request.tool_choice {
        None => None,
        Some(_) if tools.is_empty() => return Err(invalid()),
        Some(ToolChoice::None) => return Err(InferenceError::Unsupported),
        Some(ToolChoice::Auto) => Some(aws::ToolChoice::Auto(
            aws::AutoToolChoice::builder().build(),
        )),
        Some(ToolChoice::Required) => {
            Some(aws::ToolChoice::Any(aws::AnyToolChoice::builder().build()))
        }
        Some(ToolChoice::Function(name)) => {
            if !names.contains(name) {
                return Err(invalid());
            }
            Some(aws::ToolChoice::Tool(
                aws::SpecificToolChoice::builder()
                    .name(name)
                    .build()
                    .map_err(|_| invalid())?,
            ))
        }
    };
    let tools = if tools.is_empty() {
        None
    } else {
        Some(
            aws::ToolConfiguration::builder()
                .set_tools(Some(tools))
                .set_tool_choice(choice)
                .build()
                .map_err(|_| invalid())?,
        )
    };
    let mut messages: Vec<aws::Message> = Vec::new();
    let mut system = Vec::new();
    let mut pending = BTreeSet::new();
    let mut seen_ids = BTreeSet::new();
    for message in &request.messages {
        size = size
            .checked_add(message.content.as_ref().map_or(0, String::len))
            .ok_or_else(invalid)?;
        if size > OUTPUT_LIMIT {
            return Err(invalid());
        }
        if message.role == Role::Developer {
            return Err(InferenceError::Unsupported);
        }
        if message.role == Role::System {
            if !messages.is_empty()
                || !message.tool_calls.is_empty()
                || message.tool_call_id.is_some()
            {
                return Err(invalid());
            }
            let text = message
                .content
                .as_ref()
                .filter(|s| !s.is_empty())
                .ok_or_else(invalid)?;
            system.push(aws::SystemContentBlock::Text(text.clone()));
            continue;
        }
        if message.role != Role::Tool && !pending.is_empty() {
            return Err(invalid());
        }
        if message.role != Role::Tool && message.tool_call_id.is_some() {
            return Err(invalid());
        }
        if message.role != Role::Assistant && !message.tool_calls.is_empty() {
            return Err(invalid());
        }
        let role = if message.role == Role::Assistant {
            aws::ConversationRole::Assistant
        } else {
            aws::ConversationRole::User
        };
        let mut content = Vec::new();
        if message.role == Role::Tool {
            let id = message.tool_call_id.as_ref().ok_or_else(invalid)?;
            if !pending.remove(id) {
                return Err(invalid());
            }
            // Canonical tool result content is a string. Preserve it verbatim, including JSON
            // strings; parsing/re-emitting would alter its semantics. Tool-use input is JSON.
            let text = message.content.as_ref().ok_or_else(invalid)?;
            content.push(aws::ContentBlock::ToolResult(
                aws::ToolResultBlock::builder()
                    .tool_use_id(id)
                    .content(aws::ToolResultContentBlock::Text(text.clone()))
                    .build()
                    .map_err(|_| invalid())?,
            ));
        } else {
            if let Some(text) = &message.content
                && !text.is_empty()
            {
                content.push(aws::ContentBlock::Text(text.clone()));
            }
            if message.tool_calls.len() > MAX_TOOLS {
                return Err(invalid());
            }
            for call in &message.tool_calls {
                if !identifier(&call.id)
                    || !identifier(&call.name)
                    || !seen_ids.insert(call.id.clone())
                {
                    return Err(invalid());
                }
                let args: Value = serde_json::from_str(&call.arguments).map_err(|_| invalid())?;
                if !args.is_object() {
                    return Err(invalid());
                }
                size = size.checked_add(call.arguments.len()).ok_or_else(invalid)?;
                pending.insert(call.id.clone());
                content.push(aws::ContentBlock::ToolUse(
                    aws::ToolUseBlock::builder()
                        .tool_use_id(&call.id)
                        .name(&call.name)
                        .input(document(&args)?)
                        .build()
                        .map_err(|_| invalid())?,
                ));
            }
        }
        if content.is_empty() {
            return Err(invalid());
        }
        // Tool results from one assistant turn must be sent together as one user message.
        // Do not otherwise collapse user/assistant boundaries or alter turn ordering.
        if message.role == Role::Tool
            && messages.last().is_some_and(|m| {
                m.role == aws::ConversationRole::User
                    && m.content
                        .iter()
                        .all(|b| matches!(b, aws::ContentBlock::ToolResult(_)))
            })
        {
            messages.last_mut().unwrap().content.extend(content);
        } else {
            messages.push(
                aws::Message::builder()
                    .role(role)
                    .set_content(Some(content))
                    .build()
                    .map_err(|_| invalid())?,
            );
        }
    }
    if !pending.is_empty()
        || messages.is_empty()
        || messages[0].role != aws::ConversationRole::User
        || messages
            .last()
            .is_some_and(|m| m.role != aws::ConversationRole::User)
        || size > OUTPUT_LIMIT
    {
        return Err(invalid());
    }
    Ok(Input {
        messages,
        system: (!system.is_empty()).then_some(system),
        tools,
        inference: aws::InferenceConfiguration::builder()
            .set_temperature(request.temperature.map(|n| n as f32))
            .set_max_tokens(request.max_output_tokens.map(|n| n as i32))
            .build(),
    })
}

fn identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

fn document(value: &Value) -> Result<Document> {
    Ok(match value {
        Value::Null => Document::Null,
        Value::Bool(v) => Document::Bool(*v),
        Value::String(v) => Document::String(v.clone()),
        Value::Array(v) => Document::Array(v.iter().map(document).collect::<Result<_>>()?),
        Value::Object(v) => Document::Object(
            v.iter()
                .map(|(k, v)| Ok((k.clone(), document(v)?)))
                .collect::<Result<_>>()?,
        ),
        Value::Number(v) => Document::Number(if let Some(n) = v.as_u64() {
            Number::PosInt(n)
        } else if let Some(n) = v.as_i64() {
            Number::NegInt(n)
        } else {
            Number::Float(v.as_f64().ok_or(InferenceError::InvalidRequest)?)
        }),
    })
}
fn json_document(value: &Document) -> Result<Value> {
    Ok(match value {
        Document::Null => Value::Null,
        Document::Bool(v) => Value::Bool(*v),
        Document::String(v) => Value::String(v.clone()),
        Document::Array(v) => Value::Array(v.iter().map(json_document).collect::<Result<_>>()?),
        Document::Object(v) => Value::Object(
            v.iter()
                .map(|(k, v)| Ok((k.clone(), json_document(v)?)))
                .collect::<Result<_>>()?,
        ),
        Document::Number(Number::PosInt(v)) => Value::from(*v),
        Document::Number(Number::NegInt(v)) => Value::from(*v),
        Document::Number(Number::Float(v)) => {
            Value::Number(serde_json::Number::from_f64(*v).ok_or(InferenceError::InvalidUpstream)?)
        }
    })
}
fn finish(reason: &aws::StopReason) -> Result<FinishReason> {
    match reason.as_str() {
        "end_turn" | "stop_sequence" => Ok(FinishReason::Stop),
        "max_tokens" => Ok(FinishReason::Length),
        "tool_use" => Ok(FinishReason::ToolCalls),
        "guardrail_intervened" | "content_filtered" => Ok(FinishReason::ContentFilter),
        _ => Err(InferenceError::InvalidUpstream),
    }
}
fn usage(value: &aws::TokenUsage) -> Result<Usage> {
    if value.total_tokens < 0
        || value.cache_read_input_tokens.is_some_and(|n| n < 0)
        || value.cache_write_input_tokens.is_some_and(|n| n < 0)
    {
        return Err(InferenceError::InvalidUpstream);
    }
    let mut wire =
        serde_json::json!({"inputTokens":value.input_tokens,"outputTokens":value.output_tokens});
    if let Some(n) = value.cache_read_input_tokens {
        wire["cacheReadInputTokens"] = n.into();
    }
    if let Some(n) = value.cache_write_input_tokens {
        wire["cacheWriteInputTokens"] = n.into();
    }
    if let Some(details) = &value.cache_details {
        wire["cacheDetails"] = Value::Array(
            details
                .iter()
                .map(|d| serde_json::json!({"ttl":d.ttl.as_str(),"inputTokens":d.input_tokens}))
                .collect(),
        );
    }
    super::metering::bedrock(&wire)
}
fn decode_complete(
    output: aws_sdk_bedrockruntime::operation::converse::ConverseOutput,
) -> Result<ChatResponse> {
    let Some(aws::ConverseOutput::Message(message)) = output.output else {
        return Err(InferenceError::InvalidUpstream);
    };
    if message.role != aws::ConversationRole::Assistant || message.content.len() > MAX_BLOCKS {
        return Err(InferenceError::InvalidUpstream);
    }
    let mut text = String::new();
    let mut calls = Vec::new();
    let mut ids = BTreeSet::new();
    let mut size = 0;
    for block in message.content {
        match block {
            aws::ContentBlock::Text(value) => {
                size += value.len();
                text.push_str(&value);
            }
            aws::ContentBlock::ToolUse(value) => {
                if !identifier(&value.tool_use_id)
                    || !identifier(&value.name)
                    || !ids.insert(value.tool_use_id.clone())
                    || calls.len() >= MAX_TOOLS
                {
                    return Err(InferenceError::InvalidUpstream);
                }
                let args = json_document(&value.input)?;
                if !args.is_object() {
                    return Err(InferenceError::InvalidUpstream);
                }
                let arguments =
                    serde_json::to_string(&args).map_err(|_| InferenceError::InvalidUpstream)?;
                size += arguments.len() + value.name.len() + value.tool_use_id.len();
                calls.push(ToolCall {
                    id: value.tool_use_id,
                    name: value.name,
                    arguments,
                });
            }
            _ => return Err(InferenceError::InvalidUpstream),
        }
        if size > OUTPUT_LIMIT {
            return Err(InferenceError::InvalidUpstream);
        }
    }
    let reason = finish(&output.stop_reason)?;
    if (reason == FinishReason::ToolCalls) != !calls.is_empty() {
        return Err(InferenceError::InvalidUpstream);
    }
    let mut usage = output
        .usage
        .as_ref()
        .map(usage)
        .transpose()?
        .unwrap_or_default();
    usage.reported_model =
        invoked_model(output.trace.as_ref().and_then(|t| t.prompt_router.as_ref()));
    Ok(ChatResponse {
        content: (!text.is_empty()).then_some(text),
        tool_calls: calls,
        finish_reason: reason,
        usage,
    })
}
/// Telemetry only: Converse reports the served model only for prompt routers
/// (`trace.promptRouter.invokedModelId`). Otherwise it is unknown and Logs
/// show the configured model id. Invalid values are unknown, never an error.
fn invoked_model(trace: Option<&aws::PromptRouterTrace>) -> Option<ReportedModel> {
    trace
        .and_then(|t| t.invoked_model_id.as_deref())
        .and_then(ReportedModel::parse)
}

fn sdk_error<E: ProvideErrorMetadata>(error: SdkError<E>) -> InferenceError {
    if matches!(error, SdkError::TimeoutError(_)) {
        return InferenceError::Timeout;
    }
    if let Some(service) = error.as_service_error() {
        return service_error(service.code());
    }
    if matches!(error, SdkError::ResponseError(_)) {
        InferenceError::InvalidUpstream
    } else {
        InferenceError::UpstreamUnavailable
    }
}
fn service_error(code: Option<&str>) -> InferenceError {
    match code {
        Some(
            "AccessDeniedException"
            | "UnrecognizedClientException"
            | "InvalidSignatureException"
            | "ExpiredTokenException",
        ) => InferenceError::Configuration,
        Some("ThrottlingException" | "throttlingException" | "ServiceQuotaExceededException") => {
            InferenceError::Busy
        }
        Some("ModelTimeoutException" | "modelTimeoutException") => InferenceError::Timeout,
        Some("ValidationException" | "validationException" | "ResourceNotFoundException") => {
            InferenceError::UpstreamRejected
        }
        _ => InferenceError::UpstreamUnavailable,
    }
}
