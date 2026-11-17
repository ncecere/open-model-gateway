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
    pub(super) fn push(&mut self, event: aws::ConverseStreamOutput) -> Result<Vec<ChatEvent>> {
        use aws::ConverseStreamOutput as E;
        let mut result = Vec::new();
        match event {
            E::MessageStart(start) => self.message_start(&start)?,
            E::ContentBlockStart(start) => self.block_start(start, &mut result)?,
            E::ContentBlockDelta(delta) => self.block_delta(delta, &mut result)?,
            E::ContentBlockStop(stop) => self.block_stop(&stop)?,
            E::MessageStop(stop) => self.message_stop(&stop, &mut result)?,
            E::Metadata(metadata) => self.record_metadata(metadata, &mut result)?,
            _ => return Err(InferenceError::InvalidUpstream),
        }
        if self.size > self.limit.unwrap_or(OUTPUT_LIMIT) {
            return Err(InferenceError::InvalidUpstream);
        }
        Ok(result)
    }
    /// The one assistant `messageStart`.
    fn message_start(&mut self, start: &aws::MessageStartEvent) -> Result<()> {
        if self.started || start.role != aws::ConversationRole::Assistant {
            return Err(InferenceError::InvalidUpstream);
        }
        self.started = true;
        Ok(())
    }
    /// A new content block: text, or a tool use whose id and name are announced.
    fn block_start(
        &mut self,
        start: aws::ContentBlockStartEvent,
        result: &mut Vec<ChatEvent>,
    ) -> Result<()> {
        let invalid = || InferenceError::InvalidUpstream;
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
        Ok(())
    }
    /// A text or tool-input delta; a text delta may open its block implicitly.
    fn block_delta(
        &mut self,
        delta: aws::ContentBlockDeltaEvent,
        result: &mut Vec<ChatEvent>,
    ) -> Result<()> {
        let invalid = || InferenceError::InvalidUpstream;
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
        Ok(())
    }
    /// Closes a block; a tool block's accumulated arguments must be a JSON object.
    fn block_stop(&mut self, stop: &aws::ContentBlockStopEvent) -> Result<()> {
        let invalid = || InferenceError::InvalidUpstream;
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
            let args: Value = serde_json::from_str(&block.arguments).map_err(|_| invalid())?;
            if !args.is_object() {
                return Err(invalid());
            }
            block.arguments.clear();
        }
        block.closed = true;
        Ok(())
    }
    /// `messageStop`: every block is closed and the reason agrees with the tool calls.
    fn message_stop(
        &mut self,
        stop: &aws::MessageStopEvent,
        result: &mut Vec<ChatEvent>,
    ) -> Result<()> {
        let invalid = || InferenceError::InvalidUpstream;
        if !self.started || self.stopped || self.blocks.values().any(|b| !b.closed) {
            return Err(invalid());
        }
        let reason = finish(&stop.stop_reason)?;
        if (reason == FinishReason::ToolCalls) != (self.tools > 0) {
            return Err(invalid());
        }
        self.stopped = true;
        result.push(ChatEvent::Finish(reason));
        Ok(())
    }
    /// The one trailing `metadata` frame, with its usage.
    fn record_metadata(
        &mut self,
        metadata: aws::ConverseStreamMetadataEvent,
        result: &mut Vec<ChatEvent>,
    ) -> Result<()> {
        if !self.stopped || self.metadata {
            return Err(InferenceError::InvalidUpstream);
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
        Ok(())
    }
    pub(super) fn end(&self) -> Result<()> {
        // Bedrock has no SSE [DONE] frame. Its terminal sequence is messageStop,
        // metadata, then clean HTTP/event-stream EOF. Never turn truncated EOF into success:
        // a body that ends before the terminal sequence is a transport failure.
        if self.started && self.stopped && self.metadata {
            Ok(())
        } else {
            Err(crate::providers::framing::TRANSPORT)
        }
    }
}

/// Classifies a failed receive. An exception frame is named by its
/// `:exception-type` header, which AWS always sends; the SDK exposes it as the
/// typed variant, or (for a type the ConverseStream model does not list, or a
/// payload the SDK could not parse) in the raw frame. The JSON payload is
/// `{"message": ...}` with no code, and neither it nor the message text is
/// ever read for the class.
///
/// Everything else is not an exception frame, and the class follows where the
/// failure arose:
///
/// - the transport: a body that failed (reset, read error), ended inside a
///   frame, or carried a frame the event-stream decoder cannot decode (length,
///   CRC, headers) is `UpstreamUnavailable`, like a connection failure before
///   the stream; a transport timeout is `Timeout`. These are told apart by the
///   body's own `wire::Failure`, which the SDK wraps in a dispatch failure, and
///   by the raw message of a response error (`Invalid`: the SDK's decoder
///   rejected the bytes);
/// - the provider's protocol: a frame that decoded but means nothing to the
///   SDK (no string `:exception-type`, a `:message-type` `error` frame, a
///   modeled exception whose payload has the wrong shape, an event whose JSON
///   does not match its model), or a frame whose content the gateway rejects
///   (usage envelope, duplicate headers, the wire limit), is
///   `InvalidUpstream`, failing closed.
fn receive_error(
    error: &SdkError<aws::error::ConverseStreamOutputError, RawMessage>,
    messages: bool,
) -> InferenceError {
    use aws::error::ConverseStreamOutputError as Exception;
    let name = match error.as_service_error() {
        None => return transport_class(error),
        Some(Exception::InternalServerException(_)) => "internalServerException",
        Some(Exception::ModelStreamErrorException(_)) => "modelStreamErrorException",
        Some(Exception::ValidationException(_)) => "validationException",
        Some(Exception::ThrottlingException(_)) => "throttlingException",
        Some(Exception::ServiceUnavailableException(_)) => "serviceUnavailableException",
        // Everything else the SDK reports as a service error: a type it does
        // not model (for example `modelTimeoutException`), the SDK's
        // `Unhandled` (a payload it could not parse), and any variant a later
        // SDK adds. Only the frame names it; the SDK keeps the last of
        // repeated headers, and so does this.
        Some(_) => match error.raw_response() {
            Some(RawMessage::Decoded(frame)) => frame
                .headers()
                .iter()
                .rev()
                .find(|h| h.name().as_str() == ":exception-type")
                .and_then(|h| h.value().as_string().ok())
                .map_or("", |name| name.as_str()),
            _ => "",
        },
    };
    exception_class(name, messages)
}

/// The class of a receive failure that is not an exception frame.
fn transport_class(
    error: &SdkError<aws::error::ConverseStreamOutputError, RawMessage>,
) -> InferenceError {
    match error {
        SdkError::TimeoutError(_) => InferenceError::Timeout,
        SdkError::DispatchFailure(_) => {
            let mut source: Option<&(dyn std::error::Error + 'static)> = Some(error);
            while let Some(cause) = source {
                if let Some(failure) = cause.downcast_ref::<super::wire::Failure>() {
                    return failure.class();
                }
                source = cause.source();
            }
            InferenceError::InvalidUpstream
        }
        // The SDK's decoder could not decode the bytes (`Invalid`), or the
        // frame decoded and its content is wrong (`Decoded`).
        SdkError::ResponseError(response) => match response.raw() {
            RawMessage::Invalid(_) => InferenceError::UpstreamUnavailable,
            _ => InferenceError::InvalidUpstream,
        },
        _ => InferenceError::InvalidUpstream,
    }
}

/// The class of a ConverseStream exception name. Only the documented
/// lowerCamel names are classified; any other name, in any casing or form, is
/// an unknown failure and stays `UpstreamUnavailable`: not evidence of a
/// client fault or a configuration fault (those classes do not count against
/// routing health, and a rejection can settle as a free known zero), nor of a
/// safe capacity condition. This is deliberately not `service_error`, the
/// table for an HTTP error response.
///
/// `messages` marks the Anthropic Messages protocol, where a provider-reported
/// unavailability is an overload (HTTP 529 / `overloaded_error`); for the other
/// client protocols it stays `UpstreamUnavailable`, the split
/// `sdk_messages_error` applies to the same exception from an HTTP response.
fn exception_class(name: &str, messages: bool) -> InferenceError {
    match name {
        "throttlingException" => InferenceError::Busy,
        "validationException" => InferenceError::UpstreamRejected,
        "modelTimeoutException" => InferenceError::Timeout,
        "serviceUnavailableException" if messages => InferenceError::Overloaded,
        // `internalServerException` and `modelStreamErrorException` (AWS:
        // "Retry your request"), a non-Messages `serviceUnavailableException`,
        // and every other name.
        _ => InferenceError::UpstreamUnavailable,
    }
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

#[cfg(test)]
#[path = "stream_property_tests.rs"]
mod property_tests;

#[cfg(test)]
mod receive_tests {
    use super::*;
    #[test]
    fn sdk_timeout_and_invalid_raw_message_keep_transport_classes() {
        let timeout = SdkError::timeout_error(std::io::Error::other("timeout"));
        assert_eq!(transport_class(&timeout), InferenceError::Timeout);
        let invalid =
            SdkError::response_error(std::io::Error::other("decode"), RawMessage::invalid(None));
        assert_eq!(
            transport_class(&invalid),
            InferenceError::UpstreamUnavailable
        );
    }
}
