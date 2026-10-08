//! OpenRouter Image API: `POST /images` (image-only models are rejected on
//! chat completions).
//!
//! - Client `size` must be a resolution tier (`512|768|1K|1.5K|2K|4K`) and is
//!   sent as `resolution`; omitted means the gateway default `1K`, so the
//!   billed tier is always known. Pixel sizes and `auto` are unsupported here
//!   (the billed tier would be ambiguous). `quality` and `seed` pass through.
//! - `output_image_variant` is the tier in OpenRouter's endpoint pricing
//!   spelling (`768`, `1k`, `1.5k`, `2k`, `4k`), matching imported price lines.
//! - Usage follows the chat normalization (`prompt_tokens` inclusive);
//!   `completion_tokens` may be synthetic image tokens and are recorded as
//!   reported. `usage.cost` is exact provider-cost evidence only.
//! - `provider.data_collection` is sent like every other OpenRouter workload.
//!   The Image API schema does not list it, and live verification is blocked
//!   (402 without purchased credit), so enforcement there is unverified.
use serde_json::{Value, json};

use super::{OpenRouterAdapter, parse};
use crate::{
    inference::{
        error::InferenceError,
        images::{ImageRequest, ImageResponse, ImageSize, ImageTier},
        types::{Deployment, Usage},
    },
    providers::{images, metering},
};

type Result<T> = std::result::Result<T, InferenceError>;

/// Billed tier used when the client sends no size.
pub(super) const DEFAULT_TIER: ImageTier = ImageTier::K1;

fn tier(request: &ImageRequest) -> Option<ImageTier> {
    match request.size {
        None => Some(DEFAULT_TIER),
        Some(ImageSize::Tier(t)) => Some(t),
        Some(_) => None,
    }
}

pub(super) fn supports(request: &ImageRequest) -> bool {
    tier(request).is_some()
}

pub(super) fn usage(value: &Value, cost: Option<i64>) -> Result<Usage> {
    let mut usage = metering::inclusive(
        value,
        "prompt_tokens",
        "completion_tokens",
        "prompt_tokens_details",
        "cache_write_tokens",
    )?;
    // An observed zero prompt proves every input partition is zero.
    if let Some(b) = usage.billing.as_mut()
        && b.total_input_tokens == Some(0)
    {
        if [b.cache_read_input_tokens, b.cache_write_input_tokens]
            .into_iter()
            .flatten()
            .any(|n| n != 0)
        {
            return Err(InferenceError::InvalidUpstream);
        }
        b.uncached_input_tokens = Some(0);
        b.cache_read_input_tokens = Some(0);
        b.cache_write_input_tokens = Some(0);
        b.cache_write_default_input_tokens = Some(0);
    }
    usage.provider_cost_microusd = cost;
    Ok(usage)
}

impl OpenRouterAdapter {
    pub(super) async fn generate_images(
        &self,
        target: &Deployment,
        request: ImageRequest,
    ) -> Result<ImageResponse> {
        request.validate()?;
        let tier = tier(&request).ok_or(InferenceError::Unsupported)?;
        let mut body = json!({
            "model": target.upstream_model,
            "prompt": request.prompt,
            "n": request.n,
            "resolution": tier.as_str(),
            "provider": self.provider_preferences(),
        });
        if let Some(quality) = request.quality {
            body["quality"] = json!(quality.as_str());
        }
        if let Some(seed) = request.seed {
            body["seed"] = json!(seed);
        }
        let response = self.post(target, "/images", &body).await?;
        let bytes = images::read_bounded(response, request.max_response_bytes).await?;
        let (value, cost) = parse(&bytes)?;
        drop(bytes);
        let usage = usage(&value["usage"], cost)?;
        images::decode(&value, &request, usage, Some(tier.variant()), Ok(None))
    }
}

#[cfg(test)]
mod tests;
