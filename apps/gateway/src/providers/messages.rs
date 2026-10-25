//! Explicit Messages provider policy. Native fidelity is opt-in, never passthrough.
use crate::inference::{error::InferenceError, types::ChatRequest};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProviderClass {
    Anthropic,
    OpenAi,
    Bedrock,
    Local,
}

/// Open beta list, except unpriced long-context premiums and unsupported
/// hosted/file capabilities. Body capabilities and cache TTL prices are gated
/// independently; never reject extended-cache-ttl merely for being a beta.
pub(crate) const DENIED_BETA_PREFIXES: &[&str] = &[
    "context-1m-",
    "code-execution-",
    "web-search-",
    "web-fetch-",
    "files-api-",
    "mcp-client-",
    "computer-use-",
];

/// Denied names are reflected only by the Messages HTTP codec to support
/// Claude Code's capability recovery. Never put them in inference telemetry.
pub(crate) enum BetaHeaderError {
    Invalid,
    Denied(Vec<String>),
}
impl std::fmt::Debug for BetaHeaderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Invalid or denied anthropic-beta header")
    }
}

/// Validate every comma-separated token while retaining original header bytes.
/// Shared by the HTTP frontend and the pre-admission/dispatch adapter policy.
pub(crate) fn validate_beta_headers<'a>(
    values: impl IntoIterator<Item = &'a str>,
) -> Result<(), BetaHeaderError> {
    let invalid = || BetaHeaderError::Invalid;
    let mut tokens = 0_usize;
    let mut denied = Vec::new();
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
            let token = token.trim_matches([' ', '\t']);
            if count >= 32 || token.is_empty() {
                return Err(invalid());
            }
            if DENIED_BETA_PREFIXES.iter().any(|prefix| {
                token
                    .get(..prefix.len())
                    .is_some_and(|start| start.eq_ignore_ascii_case(prefix))
            }) {
                denied.push(token.to_owned());
            }
            Ok(count + 1)
        })?;
    }
    if denied.is_empty() {
        Ok(())
    } else {
        Err(BetaHeaderError::Denied(denied))
    }
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
        .map_err(|_| InferenceError::InvalidField("anthropic-beta"))
}
