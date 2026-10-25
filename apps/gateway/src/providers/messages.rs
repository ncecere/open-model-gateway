//! Explicit Messages provider policy. Native fidelity is opt-in, never passthrough.
use crate::inference::{error::InferenceError, types::ChatRequest};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProviderClass {
    Anthropic,
    OpenAi,
    Bedrock,
    Local,
}

/// Reviewed client-side tool/thinking/context features only. Adding a beta
/// requires a capability, price/bound and wire-contract review; no wildcards.
pub(crate) const ALLOWED_BETAS: &[&str] = &[
    "interleaved-thinking-2025-05-14",
    "fine-grained-tool-streaming-2025-05-14",
    "context-management-2025-06-27",
    "token-efficient-tools-2025-02-19",
];

/// Validate every comma-separated token while retaining original header bytes.
/// Shared by the HTTP frontend and the pre-admission/dispatch adapter policy.
pub(crate) fn validate_beta_headers<'a>(
    values: impl IntoIterator<Item = &'a str>,
) -> Result<(), InferenceError> {
    let invalid = || InferenceError::InvalidField("anthropic-beta");
    let mut tokens = 0_usize;
    for (index, value) in values.into_iter().enumerate() {
        if index >= 16
            || value.is_empty()
            || value.len() > 1024
            || !value
                .bytes()
                .all(|b| b == b'\t' || b == b' ' || b.is_ascii_graphic())
        {
            return Err(invalid());
        }
        tokens = value.split(',').try_fold(tokens, |count, token| {
            if count >= 32 || !ALLOWED_BETAS.contains(&token.trim_matches([' ', '\t'])) {
                Err(invalid())
            } else {
                Ok(count + 1)
            }
        })?;
    }
    Ok(())
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
    validate_beta_headers(options.beta_headers.iter().map(String::as_str))
}
