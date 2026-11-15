use super::*;
use aws_sdk_bedrockruntime::primitives::event_stream::EventReceiver;
use aws_smithy_types::event_stream::RawMessage;

struct Block {
    tool: Option<u32>,
    arguments: String,
    closed: bool,
}
#[derive(Default)]
pub(super) struct State {
    started: bool,
    stopped: bool,
    metadata: bool,
    size: usize,
    limit: Option<usize>,
    tools: u32,
    blocks: BTreeMap<i32, Block>,
    ids: BTreeSet<String>,
}
impl State {
    #[cfg_attr(not(test), expect(clippy::too_many_lines, reason = "#499"))]
    pub(super) fn push(&mut self, event: aws::ConverseStreamOutput) -> Result<Vec<ChatEvent>> {
        use aws::ConverseStreamOutput as E;
        let invalid = || InferenceError::InvalidUpstream;
        let mut result = Vec::new();
        match event {
            E::MessageStart(start) => {
                if self.started || start.role != aws::ConversationRole::Assistant {
                    return Err(invalid());
                }
                self.started = true;
            }
            E::ContentBlockStart(start) => {
                if !self.started
                    || self.stopped
                    || start.content_block_index < 0
                    || self.blocks.contains_key(&start.content_block_index)
                    || self.blocks.len() >= MAX_BLOCKS
                {
                    return Err(invalid());
                }
                let tool = match start.start {
                    Some(aws::ContentBlockStart::ToolUse(tool)) => {
                        if self.tools as usize >= MAX_TOOLS
                            || !identifier(&tool.tool_use_id)
                            || !identifier(&tool.name)
                            || !self.ids.insert(tool.tool_use_id.clone())
                        {
                            return Err(invalid());
                        }
                        let index = self.tools;
                        self.tools += 1;
                        self.size += tool.tool_use_id.len() + tool.name.len();
                        result.push(ChatEvent::Delta {
                            text: None,
                            tool_calls: vec![ToolCallDelta {
                                index,
                                id: Some(tool.tool_use_id),
                                name: Some(tool.name),
                                arguments: None,
                            }],
                        });
                        Some(index)
                    }
                    None => None, // Text blocks can explicitly start with {} or implicitly on first delta.
                    _ => return Err(invalid()),
                };
                self.blocks.insert(
                    start.content_block_index,
                    Block {
                        tool,
                        arguments: String::new(),
                        closed: false,
                    },
                );
            }
            E::ContentBlockDelta(delta) => {
                if !self.started || self.stopped || delta.content_block_index < 0 {
                    return Err(invalid());
                }
                if !self.blocks.contains_key(&delta.content_block_index) {
                    if self.blocks.len() >= MAX_BLOCKS
                        || !matches!(delta.delta, Some(aws::ContentBlockDelta::Text(_)))
                    {
                        return Err(invalid());
                    }
                    self.blocks.insert(
                        delta.content_block_index,
                        Block {
                            tool: None,
                            arguments: String::new(),
                            closed: false,
                        },
                    );
                }
                let block = self
                    .blocks
                    .get_mut(&delta.content_block_index)
                    .ok_or_else(invalid)?;
                if block.closed {
                    return Err(invalid());
                }
                match delta.delta {
                    Some(aws::ContentBlockDelta::Text(text)) if block.tool.is_none() => {
                        self.size += text.len();
                        result.push(ChatEvent::Delta {
                            text: Some(text),
                            tool_calls: Vec::new(),
                        });
                    }
                    Some(aws::ContentBlockDelta::ToolUse(tool)) if block.tool.is_some() => {
                        self.size += tool.input.len();
                        if self.size > self.limit.unwrap_or(OUTPUT_LIMIT) {
                            return Err(invalid());
                        }
                        block.arguments.push_str(&tool.input);
                        result.push(ChatEvent::Delta {
                            text: None,
                            tool_calls: vec![ToolCallDelta {
                                index: block.tool.unwrap(),
                                id: None,
                                name: None,
                                arguments: Some(tool.input),
                            }],
                        });
                    }
                    _ => return Err(invalid()),
                }
            }
            E::ContentBlockStop(stop) => {
                if !self.started || self.stopped {
                    return Err(invalid());
                }
                let block = self
                    .blocks
                    .get_mut(&stop.content_block_index)
                    .ok_or_else(invalid)?;
                if block.closed {
                    return Err(invalid());
                }
                if block.tool.is_some() {
                    let args: Value =
                        serde_json::from_str(&block.arguments).map_err(|_| invalid())?;
                    if !args.is_object() {
                        return Err(invalid());
                    }
                    block.arguments.clear();
                }
                block.closed = true;
            }
            E::MessageStop(stop) => {
                if !self.started || self.stopped || self.blocks.values().any(|b| !b.closed) {
                    return Err(invalid());
                }
                let reason = finish(&stop.stop_reason)?;
                if (reason == FinishReason::ToolCalls) != (self.tools > 0) {
                    return Err(invalid());
                }
                self.stopped = true;
                result.push(ChatEvent::Finish(reason));
            }
            E::Metadata(metadata) => {
                if !self.stopped || self.metadata {
                    return Err(invalid());
                }
                self.metadata = true;
                if let Some(value) = metadata.usage {
                    let mut usage = usage(&value)?;
                    usage.reported_model = invoked_model(
                        metadata
                            .trace
                            .as_ref()
                            .and_then(|t| t.prompt_router.as_ref()),
                    );
                    result.push(ChatEvent::Usage(usage));
                }
            }
            _ => return Err(invalid()),
        }
        if self.size > self.limit.unwrap_or(OUTPUT_LIMIT) {
            return Err(invalid());
        }
        Ok(result)
    }
    pub(super) fn end(&self) -> Result<()> {
        // Bedrock has no SSE [DONE] frame. Its terminal sequence is messageStop,
        // metadata, then clean HTTP/event-stream EOF. Never turn truncated EOF into success.
        if self.started && self.stopped && self.metadata {
            Ok(())
        } else {
            Err(InferenceError::InvalidUpstream)
        }
    }
}

/// Classifies a failed receive. An exception frame is named by its
/// `:exception-type` header, which AWS always sends; the SDK exposes it as the
/// typed variant, or in the raw frame for a type the ConverseStream model does
/// not list. Its JSON payload is `{"message": ...}` with no code, and neither
/// that payload nor the message text is ever read for the class.
///
/// `messages` marks the Anthropic Messages protocol, where a provider-reported
/// unavailability is an overload (HTTP 529 / `overloaded_error`); for the other
/// client protocols it stays `UpstreamUnavailable`. This is the split
/// `sdk_messages_error` applies to the same exception from an HTTP response.
/// Anything that is not an exception frame (a transport failure, a corrupt or
/// `:message-type: error` frame) fails closed as `InvalidUpstream`.
fn receive_error(
    error: &SdkError<aws::error::ConverseStreamOutputError, RawMessage>,
    messages: bool,
) -> InferenceError {
    use aws::error::ConverseStreamOutputError as Exception;
    let name = match error.as_service_error() {
        None => return InferenceError::InvalidUpstream,
        Some(Exception::InternalServerException(_)) => "internalServerException",
        Some(Exception::ModelStreamErrorException(_)) => "modelStreamErrorException",
        Some(Exception::ValidationException(_)) => "validationException",
        Some(Exception::ThrottlingException(_)) => "throttlingException",
        Some(Exception::ServiceUnavailableException(_)) => {
            return if messages {
                InferenceError::Overloaded
            } else {
                InferenceError::UpstreamUnavailable
            };
        }
        // Not modeled here (for example `modelTimeoutException`): only the
        // frame header names it. No header, or an unknown name, is unclassified.
        Some(_) => match error.raw_response() {
            Some(RawMessage::Decoded(frame)) => frame
                .headers()
                .iter()
                .find(|h| h.name().as_str() == ":exception-type")
                .and_then(|h| h.value().as_string().ok())
                .map(|name| name.as_str()),
            _ => None,
        }
        .unwrap_or_default(),
    };
    service_error(Some(name).filter(|name| !name.is_empty()))
}

pub(super) fn decode(
    mut receiver: EventReceiver<aws::ConverseStreamOutput, aws::error::ConverseStreamOutputError>,
    limit: usize,
    messages: bool,
) -> EventStream {
    Box::pin(async_stream::try_stream! {
        let mut state = State { limit: Some(limit), ..State::default() };
        loop {
            // Explicit deadline for recv: SDK operation timeout ends when response headers arrive.
            let next = tokio::time::timeout(Duration::from_secs(60), receiver.recv()).await
                .map_err(|_| InferenceError::Timeout)?
                .map_err(|error| receive_error(&error, messages))?;
            match next {
                Some(event) => for event in state.push(event)? { yield event; },
                None => { state.end()?; yield ChatEvent::Done; break; }
            }
        }
    })
}
