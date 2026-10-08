//! Opt-in, paid image acceptance through the real adapter code. Never runs in
//! CI: it is `#[ignore]` and additionally requires `IMAGES_LIVE=1` plus the
//! server-held keys `OPENAI_KEY` / `OPEN_ROUTER`. It prints only outcomes,
//! sizes and usage counters (never keys, prompts or image bytes).
//!
//! Expected spend: one `gpt-image-1-mini` low-quality 1024x1024 image
//! (about $0.005 list price). The OpenRouter call is expected to fail with
//! 402 before any provider runs (account without purchased credit); it is
//! not retried. Optional subset: `IMAGES_LIVE_ONLY=openai` or `openrouter`.
use std::sync::Arc;

use open_model_gateway::{
    inference::{error::InferenceError, types::*},
    providers::{
        ProviderAdapter,
        openai::OpenAiAdapter,
        openrouter::{OpenRouterAdapter, OpenRouterConfig},
        secrets::EnvSecrets,
    },
};

fn target(provider: &str, model: &str, key: &str) -> Deployment {
    Deployment {
        id: uuid::Uuid::new_v4(),
        provider: provider.into(),
        upstream_model: model.into(),
        credential_ref: format!("env:{key}"),
        endpoint: None,
        region: None,
        supported_protocols: vec!["images".into()],
    }
}
fn request(size: ImageSize, quality: Option<ImageQuality>) -> ImageRequest {
    ImageRequest {
        model: "live/image".into(),
        prompt: "a small red circle on a white background".into(),
        n: 1,
        size: Some(size),
        quality,
        seed: None,
        max_response_bytes: 20 * 1024 * 1024,
    }
}

#[tokio::test]
#[ignore = "live paid request: set IMAGES_LIVE=1, OPENAI_KEY and OPEN_ROUTER"]
async fn images_live_openai_mini_and_openrouter_no_credit() {
    assert_eq!(
        std::env::var("IMAGES_LIVE").as_deref(),
        Ok("1"),
        "explicit opt-in required"
    );
    let only = std::env::var("IMAGES_LIVE_ONLY").ok();
    let run = |name: &str| only.as_deref().is_none_or(|o| o == name);
    let mut failures = Vec::new();

    if run("openai") {
        assert!(std::env::var_os("OPENAI_KEY").is_some(), "OPENAI_KEY unset");
        let adapter =
            OpenAiAdapter::new(Arc::new(EnvSecrets::new(["OPENAI_KEY".to_owned()]))).unwrap();
        let t = target("openai", "gpt-image-1-mini", "OPENAI_KEY");
        let r = request(
            ImageSize::Pixels {
                width: 1024,
                height: 1024,
            },
            Some(ImageQuality::Low),
        );
        assert!(adapter.supports_image_request(&t, &r));
        let started = std::time::Instant::now();
        match adapter.execute_images(&t, r.clone()).await {
            Ok(response) => {
                let u = response.usage;
                eprintln!(
                    "LIVE openai gpt-image-1-mini: ok valid={} images={} media_type={:?} b64_len={:?} created_present={} elapsed_ms={} input_tokens={:?} output_tokens={:?} uncached={:?} cache_read={:?} meters={:?} variant={:?} provider_cost={:?}",
                    response.valid_for(&r),
                    response.images.len(),
                    response.images.first().map(|i| i.media_type),
                    response.images.first().map(|i| i.b64_json.len()),
                    response.created > 0,
                    started.elapsed().as_millis(),
                    u.input_tokens,
                    u.output_tokens,
                    u.billing.and_then(|b| b.uncached_input_tokens),
                    u.billing.and_then(|b| b.cache_read_input_tokens),
                    u.meters.map(|m| m.counts()),
                    u.output_image_variant,
                    u.provider_cost_microusd,
                );
                if !response.valid_for(&r) {
                    failures.push("openai: invalid response".to_owned());
                }
            }
            Err(e) => {
                eprintln!("LIVE openai gpt-image-1-mini: error {}", e.code());
                failures.push(format!("openai: {}", e.code()));
            }
        }
    }

    if run("openrouter") {
        assert!(
            std::env::var_os("OPEN_ROUTER").is_some(),
            "OPEN_ROUTER unset"
        );
        let adapter = OpenRouterAdapter::new(
            Arc::new(EnvSecrets::new(["OPEN_ROUTER".to_owned()])),
            OpenRouterConfig::default(),
        )
        .unwrap();
        let t = target(
            "openrouter",
            "black-forest-labs/flux-3-image",
            "OPEN_ROUTER",
        );
        let r = request(ImageSize::Tier(ImageTier::P768), None);
        match adapter.execute_images(&t, r).await {
            Err(InferenceError::Configuration) => eprintln!(
                "LIVE openrouter flux-3-image: provider_configuration_error (expected 402 without credit)"
            ),
            Err(e) => {
                eprintln!(
                    "LIVE openrouter flux-3-image: unexpected error {}",
                    e.code()
                );
                failures.push(format!("openrouter: {}", e.code()));
            }
            Ok(response) => {
                eprintln!(
                    "LIVE openrouter flux-3-image: unexpected success images={} variant={:?} provider_cost={:?}",
                    response.images.len(),
                    response.usage.output_image_variant,
                    response.usage.provider_cost_microusd
                );
                failures.push("openrouter: expected 402".to_owned());
            }
        }
    }
    assert!(failures.is_empty(), "{failures:?}");
}
