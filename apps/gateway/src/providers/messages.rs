//! Frontend delivery policy (#70). Provider fidelity is opt-in, never passthrough.
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
    if !options.images.is_empty() {
        return refuse("image");
    }
    if options
        .output_config
        .as_ref()
        .is_some_and(|v| v.get("format").is_some())
    {
        return refuse("output_config.format");
    }
    // This is a Messages wire-field refusal, not a new rule for shared
    // FunctionTool.strict values supplied by existing canonical adapter callers.
    if options.strict_tools {
        return refuse("strict");
    }
    if class == ProviderClass::Anthropic {
        // These MUST be forwarded on native routes. Until PR 2 can represent
        // them faithfully, refusal is the only safe conservative rule.
        if options.beta {
            return refuse("anthropic-beta");
        }
        if !options.cache_controls.is_empty() {
            return refuse("cache_control");
        }
        if options.thinking.is_some()
            || options
                .history_thinking
                .iter()
                .any(|b| b["type"] == "thinking")
        {
            return refuse("thinking");
        }
        if !options.history_thinking.is_empty() {
            return refuse("redacted_thinking");
        }
        if options
            .output_config
            .as_ref()
            .is_some_and(|v| v.get("effort").is_some())
        {
            return refuse("output_config.effort");
        }
        if options.context_management.is_some() {
            return refuse("context_management");
        }
        if !options.defer_loading.is_empty() {
            return refuse("defer_loading");
        }
        if options.task_budget.is_some() {
            return refuse("output_config.task_budget");
        }
    } else {
        // No adapter in this delivery declares a reasoning control. Validated
        // thinking/effort, cache/context/task hints and foreign thinking history
        // are intentionally not encoded. PR 3 opts into reasoning mapping.
        if options.top_k.is_some() {
            return refuse("top_k");
        }
    }
    Ok(())
}
