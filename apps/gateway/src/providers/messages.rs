//! Explicit Messages provider policy. Native fidelity is opt-in, never passthrough.
use crate::inference::{error::InferenceError, types::ChatRequest};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProviderClass {
    Anthropic,
    OpenAi,
    Bedrock,
    Local,
}

pub(crate) fn validate(request: &ChatRequest, class: ProviderClass) -> Result<(), InferenceError> {
    let Some(options) = &request.messages_options else {
        return Ok(());
    };
    let refuse = |field| Err(InferenceError::InvalidField(field));
    if class != ProviderClass::Anthropic && !options.images.is_empty() {
        return refuse("image");
    }
    if options
        .output_config
        .as_ref()
        .is_some_and(|v| v.get("format").is_some())
    {
        return refuse("output_config.format");
    }
    if options.strict_tools {
        return refuse("strict");
    }
    if class != ProviderClass::Anthropic {
        // Error-marked results cannot be represented faithfully by shared text
        // results. Native Anthropic retains the original boolean, including false.
        if options.error_results {
            return Err(InferenceError::Unsupported);
        }
        if !options.history_thinking.is_empty()
            && request
                .messages
                .iter()
                .all(|m| m.content.is_none() && m.tool_calls.is_empty())
        {
            // Preserve the previous translated refusal when removing foreign
            // history leaves no text/tool conversation at all.
            return Err(InferenceError::InvalidRequest);
        }
        // Reasoning mapping is delivery 3. Hints and foreign signed thinking
        // are deliberately omitted by translated providers, not forwarded.
        if options.top_k.is_some() {
            return refuse("top_k");
        }
    }
    if options.beta_headers.len() > 16
        || options.beta_headers.iter().any(|s| {
            s.is_empty() || s.len() > 1024 || !s.bytes().all(|b| b == b' ' || b.is_ascii_graphic())
        })
    {
        return refuse("anthropic-beta");
    }
    Ok(())
}
