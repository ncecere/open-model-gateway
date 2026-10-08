use super::*;
use crate::{
    inference::images::{ImageMediaType, ImageQuality, fixtures::*},
    providers::{
        ProviderAdapter,
        images::mock::Mock,
        openrouter::{BASE, OpenRouterConfig},
        secrets::{Secret, SecretResolver},
    },
};
use std::sync::Arc;

struct Resolver;
impl SecretResolver for Resolver {
    fn resolve(&self, reference: &str) -> Result<Secret> {
        assert_eq!(reference, "env:OPENROUTER_TEST_KEY");
        Secret::new("local-mock-openrouter-key".into())
    }
}
fn adapter(mock: &Mock) -> OpenRouterAdapter {
    OpenRouterAdapter::for_test(
        Arc::new(Resolver),
        OpenRouterConfig::default(),
        mock.base.clone(),
    )
}
fn target() -> Deployment {
    Deployment {
        id: uuid::Uuid::new_v4(),
        provider: "openrouter".into(),
        upstream_model: "black-forest-labs/flux-3-image".into(),
        credential_ref: "env:OPENROUTER_TEST_KEY".into(),
        endpoint: Some(BASE.into()),
        region: None,
        supported_protocols: vec!["images".into()],
    }
}
fn request(size: Option<ImageSize>) -> ImageRequest {
    ImageRequest {
        model: "company/flux".into(),
        prompt: "a red dot".into(),
        n: 1,
        size,
        quality: None,
        seed: None,
        max_response_bytes: 20 * 1024 * 1024,
    }
}
/// Documented `ImageGenerationResponse` (research §3).
fn body(images: &[(&str, &str)]) -> Value {
    json!({
        "created": 1791432289,
        "data": images.iter().map(|(b, t)| json!({"b64_json": b, "media_type": t})).collect::<Vec<_>>(),
        "usage": {"prompt_tokens": 0, "completion_tokens": 4175, "total_tokens": 4175, "cost": 0.0205,
                  "completion_tokens_details": {"image_tokens": 0}, "is_byok": false}
    })
}

#[tokio::test]
async fn wire_tier_variant_usage_and_exact_cost() {
    let mock = Mock::json("/api/v1", body(&[(PNG_1X1, "image/png")])).await;
    let mut r = request(Some(ImageSize::Tier(ImageTier::P768)));
    r.seed = Some(42);
    r.quality = Some(ImageQuality::High);
    let response = adapter(&mock).execute_images(&target(), r).await.unwrap();
    let (headers, path, sent) = mock.last();
    assert_eq!(path, "/api/v1/images");
    assert_eq!(headers["authorization"], "Bearer local-mock-openrouter-key");
    assert_eq!(
        sent,
        json!({"model":"black-forest-labs/flux-3-image","prompt":"a red dot","n":1,
            "resolution":"768","quality":"high","seed":42,"provider":{"data_collection":"deny"}})
    );
    assert_eq!(response.images[0].media_type, ImageMediaType::Png);
    let u = response.usage;
    assert_eq!((u.input_tokens, u.output_tokens), (Some(0), Some(4175)));
    assert_eq!(
        u.billing.unwrap().counts(),
        [
            Some(0),
            Some(0),
            Some(0),
            Some(0),
            Some(0),
            Some(0),
            Some(0)
        ]
    );
    assert_eq!(
        u.meters.unwrap().counts(),
        [Some(1), Some(0), Some(0), Some(0), Some(0), Some(1)]
    );
    assert_eq!(u.output_image_variant.unwrap().as_str(), "768");
    assert_eq!(u.provider_cost_microusd, Some(20_500));
    // No size: the gateway's documented default tier is sent and billed.
    let mock = Mock::json("/api/v1", body(&[(JPEG_1X1, "image/jpeg")])).await;
    let response = adapter(&mock)
        .execute_images(&target(), request(None))
        .await
        .unwrap();
    assert_eq!(mock.last().2["resolution"], "1K");
    assert_eq!(response.usage.output_image_variant.unwrap().as_str(), "1k");
    for (tier, variant) in [
        (ImageTier::K1_5, "1.5k"),
        (ImageTier::K2, "2k"),
        (ImageTier::K4, "4k"),
    ] {
        let mock = Mock::json("/api/v1", body(&[(PNG_1X1, "image/png")])).await;
        let response = adapter(&mock)
            .execute_images(&target(), request(Some(ImageSize::Tier(tier))))
            .await
            .unwrap();
        assert_eq!(mock.last().2["resolution"], tier.as_str());
        assert_eq!(
            response.usage.output_image_variant.unwrap().as_str(),
            variant
        );
    }
}

#[tokio::test]
async fn ambiguous_sizes_are_unsupported_before_network() {
    let mock = Mock::json("/api/v1", body(&[(PNG_1X1, "image/png")])).await;
    let a = adapter(&mock);
    for size in [
        ImageSize::Auto,
        ImageSize::Pixels {
            width: 1024,
            height: 1024,
        },
    ] {
        let r = request(Some(size));
        assert!(!a.supports_image_request(&target(), &r));
        assert_eq!(
            a.execute_images(&target(), r).await.err(),
            Some(InferenceError::Unsupported)
        );
    }
    assert!(a.supports_image_request(&target(), &request(None)));
    assert_eq!(mock.count(), 0);
}

#[tokio::test]
async fn invalid_media_counts_and_urls_fail_keeping_cost_evidence() {
    let invalid = Some(InferenceError::InvalidUpstream);
    let mut two = request(None);
    two.n = 2;
    for (value, r) in [
        (body(&[(PNG_1X1, "image/png")]), two),
        (body(&[(PNG_1X1, "image/svg+xml")]), request(None)),
        (body(&[(PNG_1X1, "image/jpeg")]), request(None)),
        (body(&[("bm90IGFuIGltYWdl", "image/png")]), request(None)),
        (body(&[("%%%%", "image/png")]), request(None)),
    ] {
        let mock = Mock::json("/api/v1", value).await;
        let (out, seen) =
            crate::inference::evidence::capture(adapter(&mock).execute_images(&target(), r)).await;
        assert_eq!(out.err(), invalid);
        assert_eq!(seen.unwrap().provider_cost_microusd, Some(20_500));
    }
    let mut url = body(&[]);
    url["data"] = json!([{"url": "https://cdn.example/x.png"}]);
    let mock = Mock::json("/api/v1", url).await;
    assert_eq!(
        adapter(&mock)
            .execute_images(&target(), request(None))
            .await
            .err(),
        invalid
    );
}

#[tokio::test]
async fn no_credit_402_and_errors_are_sanitized() {
    // Body carries the account identifier; it is never read.
    let leaky = br#"{"error":{"code":402,"message":"Insufficient credits","metadata":{"limit_source":"openrouter_credits"}},"user_id":"user_2xSecret"}"#;
    for (status, expected) in [
        (402, InferenceError::Configuration),
        (401, InferenceError::Configuration),
        (403, InferenceError::UpstreamRejected),
        (400, InferenceError::UpstreamRejected),
        (429, InferenceError::Busy),
        (524, InferenceError::Timeout),
        (503, InferenceError::UpstreamUnavailable),
    ] {
        let mock = Mock::serve("/api/v1", status, leaky.to_vec()).await;
        let error = adapter(&mock)
            .execute_images(&target(), request(None))
            .await
            .err()
            .unwrap();
        assert_eq!(error, expected, "{status}");
        assert!(!error.to_string().contains("user_2x"));
        assert_eq!(mock.count(), 1, "never retried");
    }
    // A 200 carrying `error` is a failure by code.
    let mock = Mock::serve("/api/v1", 200, leaky.to_vec()).await;
    assert_eq!(
        adapter(&mock)
            .execute_images(&target(), request(None))
            .await
            .err(),
        Some(InferenceError::Configuration)
    );
}

#[tokio::test]
async fn response_cap_applies() {
    // The 1x1 JPEG body exceeds a 1 KiB cap.
    let mock = Mock::json("/api/v1", body(&[(JPEG_1X1, "image/jpeg")])).await;
    let mut r = request(None);
    r.max_response_bytes = 1024;
    assert_eq!(
        adapter(&mock).execute_images(&target(), r).await.err(),
        Some(InferenceError::InvalidUpstream)
    );
}

#[tokio::test]
async fn dropping_the_request_cancels_upstream() {
    let dropped = Arc::new(tokio::sync::Notify::new());
    let mock = Mock::pending("/api/v1", dropped.clone()).await;
    let a = adapter(&mock);
    let t = target();
    let mut future = Box::pin(a.execute_images(&t, request(None)));
    tokio::select! {
        _ = &mut future => panic!("request unexpectedly completed"),
        _ = async { while mock.count() == 0 { tokio::task::yield_now().await } } => {}
    }
    drop(future);
    tokio::time::timeout(std::time::Duration::from_secs(3), dropped.notified())
        .await
        .expect("drop must cancel upstream");
}

#[test]
fn usage_zero_prompt_proves_zero_partitions() {
    let u = usage(&json!({"prompt_tokens":0,"completion_tokens":5}), None).unwrap();
    assert_eq!(u.billing.unwrap().uncached_input_tokens, Some(0));
    let u = usage(&json!({"prompt_tokens":7,"completion_tokens":5}), Some(3)).unwrap();
    assert_eq!(u.billing.unwrap().uncached_input_tokens, None);
    assert_eq!(u.provider_cost_microusd, Some(3));
    let u = usage(&Value::Null, None).unwrap();
    assert_eq!((u.input_tokens, u.billing), (None, None));
}
