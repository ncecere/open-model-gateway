//! OpenAI `POST /v1/images/generations` for the `gpt-image-*` family.
//!
//! - Subset: `model, prompt, n, size (auto|1024x1024|1536x1024|1024x1536),
//!   quality (auto|low|medium|high)`. gpt-image always returns base64, so no
//!   `response_format` is sent; `seed` and tiers are unsupported (rejected
//!   before admission).
//! - Usage (live research probe): `input_tokens` (text + image input),
//!   `output_tokens` (image tokens), optional per-modality details. The Images
//!   API defines no cache categories: cache reads are `cached_tokens` when
//!   reported, otherwise zero; cache writes are zero.
//! - `output_image_variant` is the generated pixel size (e.g. `1024x1024`).
use reqwest::header;
use serde_json::{Value, json};

use super::{BASE, OpenAiAdapter, check_status, transport_error};
use crate::{
    billing::BillingUsage,
    inference::{
        error::InferenceError,
        images::{ImageMediaType, ImageRequest, ImageResponse, ImageSize},
        types::{Deployment, Usage},
    },
    providers::{images, metering::count},
};

type Result<T> = std::result::Result<T, InferenceError>;

const SIZES: [(u32, u32); 3] = [(1024, 1024), (1536, 1024), (1024, 1536)];

fn supported_size(size: ImageSize) -> bool {
    match size {
        ImageSize::Auto => true,
        ImageSize::Pixels { width, height } => SIZES.contains(&(width, height)),
        ImageSize::Tier(_) => false,
    }
}

pub(super) fn supports(target: &Deployment, request: &ImageRequest) -> bool {
    target.upstream_model.starts_with("gpt-image-")
        && request.seed.is_none()
        && request.size.is_none_or(supported_size)
}

pub(super) fn encode(model: &str, request: &ImageRequest) -> Value {
    let mut body = json!({"model": model, "prompt": request.prompt, "n": request.n});
    if let Some(size) = request.size {
        body["size"] = json!(size.to_wire());
    }
    if let Some(quality) = request.quality {
        body["quality"] = json!(quality.as_str());
    }
    body
}

fn detail_sum(details: &Value, total: Option<u64>) -> Result<()> {
    if details.is_null() {
        return Ok(());
    }
    if !details.is_object() {
        return Err(InferenceError::InvalidUpstream);
    }
    let text = count(&details["text_tokens"])?;
    let image = count(&details["image_tokens"])?;
    if let (Some(t), Some(i), Some(total)) = (text, image, total)
        && t.checked_add(i) != Some(total)
    {
        return Err(InferenceError::InvalidUpstream);
    }
    Ok(())
}

pub(super) fn usage(value: &Value) -> Result<Usage> {
    if value.is_null() {
        return Ok(Usage::default());
    }
    if !value.is_object() {
        return Err(InferenceError::InvalidUpstream);
    }
    let input = count(&value["input_tokens"])?;
    let output = count(&value["output_tokens"])?;
    let details = &value["input_tokens_details"];
    detail_sum(details, input)?;
    detail_sum(&value["output_tokens_details"], output)?;
    if let (Some(total), Some(i), Some(o)) = (count(&value["total_tokens"])?, input, output)
        && i.checked_add(o) != Some(total)
    {
        return Err(InferenceError::InvalidUpstream);
    }
    let cached = count(&details["cached_tokens"])?.unwrap_or(0);
    let uncached = input
        .map(|i| i.checked_sub(cached).ok_or(InferenceError::InvalidUpstream))
        .transpose()?;
    let billing = BillingUsage {
        total_input_tokens: input,
        uncached_input_tokens: uncached,
        cache_read_input_tokens: Some(cached),
        cache_write_input_tokens: Some(0),
        cache_write_default_input_tokens: Some(0),
        cache_write_5m_input_tokens: Some(0),
        cache_write_1h_input_tokens: Some(0),
    };
    billing
        .validate()
        .map_err(|_| InferenceError::InvalidUpstream)?;
    Ok(Usage {
        input_tokens: input,
        output_tokens: output,
        billing: Some(billing),
        ..Default::default()
    })
}

pub(super) async fn execute(
    adapter: &OpenAiAdapter,
    target: &Deployment,
    request: ImageRequest,
) -> Result<ImageResponse> {
    request.validate()?;
    if !supports(target, &request) {
        return Err(InferenceError::Unsupported);
    }
    if target.provider != "openai"
        || target.credential_ref == "none"
        || target
            .endpoint
            .as_deref()
            .is_some_and(|v| v != BASE && v != "https://api.openai.com/v1/")
        || target.region.as_deref().is_some_and(|v| !v.is_empty())
    {
        return Err(InferenceError::Configuration);
    }
    let secret = adapter.resolver.resolve(&target.credential_ref)?;
    let mut auth = header::HeaderValue::from_str(&format!("Bearer {}", secret.expose()))
        .map_err(|_| InferenceError::Configuration)?;
    auth.set_sensitive(true);
    let response = adapter
        .client
        .post(format!("{}/images/generations", adapter.base))
        .header(header::AUTHORIZATION, auth)
        .json(&encode(&target.upstream_model, &request))
        .send()
        .await
        .map_err(transport_error)?;
    check_status(response.status())?;
    let bytes = images::read_bounded(response, request.max_response_bytes).await?;
    let value: Value =
        serde_json::from_slice(&bytes).map_err(|_| InferenceError::InvalidUpstream)?;
    drop(bytes);
    if !value.is_object() || !value["error"].is_null() {
        return Err(InferenceError::InvalidUpstream);
    }
    let usage = usage(&value["usage"])?;
    // An unknown declared format fails the attempt but keeps usage evidence.
    let format = match &value["output_format"] {
        Value::Null => Ok(None),
        v => v
            .as_str()
            .and_then(ImageMediaType::parse_format)
            .map(Some)
            .ok_or(InferenceError::InvalidUpstream),
    };
    // Variant: the generated size when echoed, else the explicit request size.
    let variant = value["size"]
        .as_str()
        .and_then(ImageSize::parse)
        .or(request.size)
        .filter(|s| matches!(s, ImageSize::Pixels { .. }) && supported_size(*s))
        .map(ImageSize::to_wire);
    images::decode(&value, &request, usage, variant.as_deref(), format)
}

#[cfg(test)]
mod tests;
