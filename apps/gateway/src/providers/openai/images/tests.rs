use super::*;
use crate::{
    inference::images::{ImageQuality, ImageTier, decode_base64, fixtures::*},
    providers::{
        ProviderAdapter,
        images::mock::Mock,
        secrets::{Secret, SecretResolver},
    },
};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

#[derive(Default)]
struct Resolver {
    calls: AtomicUsize,
}
impl SecretResolver for Resolver {
    fn resolve(&self, reference: &str) -> Result<Secret> {
        assert_eq!(reference, "test-key-reference");
        self.calls.fetch_add(1, Ordering::SeqCst);
        Secret::new("local-mock-key".into())
    }
}
fn adapter(mock: &Mock) -> OpenAiAdapter {
    OpenAiAdapter::for_test(Arc::new(Resolver::default()), mock.base.clone())
}
fn target() -> Deployment {
    Deployment {
        id: uuid::Uuid::new_v4(),
        provider: "openai".into(),
        upstream_model: "gpt-image-1-mini".into(),
        credential_ref: "test-key-reference".into(),
        endpoint: None,
        region: None,
        supported_protocols: vec!["images".into()],
    }
}
fn request(n: u32) -> ImageRequest {
    ImageRequest {
        model: "company/image".into(),
        prompt: "a red dot".into(),
        n,
        size: Some(ImageSize::Pixels {
            width: 1024,
            height: 1024,
        }),
        quality: Some(ImageQuality::Low),
        seed: None,
        max_response_bytes: 20 * 1024 * 1024,
    }
}
/// The live `gpt-image-1-mini` shape from the research probe.
fn body(images: &[&str], format: &str) -> Value {
    json!({
        "created": 1791432289, "background": "opaque", "output_format": format,
        "quality": "low", "size": "1024x1024",
        "data": images.iter().map(|b| json!({"b64_json": b, "generation_id": "defbf505"})).collect::<Vec<_>>(),
        "usage": {"input_tokens": 9, "input_tokens_details": {"image_tokens": 0, "text_tokens": 9},
                  "output_tokens": 272, "output_tokens_details": {"image_tokens": 272, "text_tokens": 0},
                  "total_tokens": 281}
    })
}

#[tokio::test]
async fn wire_payload_auth_usage_meters_and_variant() {
    let mock = Mock::json("/v1", body(&[PNG_1X1], "png")).await;
    let response = adapter(&mock)
        .execute_images(&target(), request(1))
        .await
        .unwrap();
    let (headers, path, sent) = mock.last();
    assert_eq!(path, "/v1/images/generations");
    assert_eq!(headers["authorization"], "Bearer local-mock-key");
    assert_eq!(
        sent,
        json!({"model":"gpt-image-1-mini","prompt":"a red dot","n":1,"size":"1024x1024","quality":"low"})
    );
    assert_eq!(response.created, 1791432289);
    assert_eq!(response.images.len(), 1);
    assert_eq!(response.images[0].media_type, ImageMediaType::Png);
    assert_eq!(response.images[0].b64_json, PNG_1X1);
    let u = response.usage;
    assert_eq!((u.input_tokens, u.output_tokens), (Some(9), Some(272)));
    let b = u.billing.unwrap();
    assert_eq!(
        b.counts(),
        [
            Some(9),
            Some(9),
            Some(0),
            Some(0),
            Some(0),
            Some(0),
            Some(0)
        ]
    );
    assert_eq!(
        u.meters.unwrap().counts(),
        [Some(1), Some(0), Some(0), Some(0), Some(0), Some(1), None]
    );
    assert_eq!(u.output_image_variant.unwrap().as_str(), "1024x1024");
    assert_eq!(u.provider_cost_microusd, None);
    assert!(response.valid_for(&request(1)));
    // `auto`/absent size: the echoed size is the variant; no size is sent.
    let mock = Mock::json("/v1", body(&[JPEG_1X1, JPEG_1X1], "jpeg")).await;
    let mut r = request(2);
    r.size = None;
    r.quality = None;
    let response = adapter(&mock).execute_images(&target(), r).await.unwrap();
    assert_eq!(
        mock.last().2,
        json!({"model":"gpt-image-1-mini","prompt":"a red dot","n":2})
    );
    assert_eq!(response.images[1].media_type, ImageMediaType::Jpeg);
    assert_eq!(
        response.usage.output_image_variant.unwrap().as_str(),
        "1024x1024"
    );
    assert_eq!(response.usage.meters.unwrap().output_images, Some(2));
}

#[tokio::test]
async fn invalid_images_counts_and_urls_fail_with_evidence() {
    let invalid = Some(InferenceError::InvalidUpstream);
    for (value, n) in [
        // Wrong count.
        (body(&[PNG_1X1], "png"), 2),
        (body(&[PNG_1X1, PNG_1X1], "png"), 1),
        // Bad base64 / not an image / declared format mismatch.
        (body(&["@@@@"], "png"), 1),
        (body(&["aGVsbG8gd29ybGQh"], "png"), 1),
        (body(&[PNG_1X1], "jpeg"), 1),
        (body(&[PNG_1X1], "gif"), 1),
    ] {
        let mock = Mock::json("/v1", value).await;
        let (out, seen) = crate::inference::evidence::capture(
            adapter(&mock).execute_images(&target(), request(n)),
        )
        .await;
        assert_eq!(out.err(), invalid);
        // The provider produced (and charged for) the images: keep tokens.
        let seen = seen.unwrap();
        assert_eq!(seen.output_tokens, Some(272));
        assert_eq!(seen.meters.unwrap().requests, Some(1));
    }
    // A URL response is never accepted (the gateway does not host files).
    let mut url = body(&[PNG_1X1], "png");
    url["data"] = json!([{"url": "https://files.example/img.png"}]);
    let mock = Mock::json("/v1", url).await;
    assert_eq!(
        adapter(&mock)
            .execute_images(&target(), request(1))
            .await
            .err(),
        invalid
    );
    // Malformed usage and embedded errors.
    let mut bad_usage = body(&[PNG_1X1], "png");
    bad_usage["usage"]["total_tokens"] = json!(1);
    let mut errored = body(&[PNG_1X1], "png");
    errored["error"] = json!({"message": "secret upstream detail"});
    for value in [bad_usage, errored, json!([1])] {
        let mock = Mock::json("/v1", value).await;
        assert_eq!(
            adapter(&mock)
                .execute_images(&target(), request(1))
                .await
                .err(),
            invalid
        );
    }
}

#[tokio::test]
async fn response_cap_is_separate_and_configurable() {
    // ~6.7 MB of base64: over the shared 4 MiB provider cap, under 20 MiB.
    let mut png = decode_base64(PNG_1X1).unwrap();
    png.resize(5 * 1024 * 1024, 0);
    let big = encode_base64(&png);
    let value = body(&[&big], "png");
    let mock = Mock::json("/v1", value.clone()).await;
    let response = adapter(&mock)
        .execute_images(&target(), request(1))
        .await
        .unwrap();
    assert_eq!(response.images[0].b64_json.len(), big.len());
    let mock = Mock::json("/v1", value).await;
    let mut small = request(1);
    small.max_response_bytes = 4 * 1024 * 1024;
    assert_eq!(
        adapter(&mock).execute_images(&target(), small).await.err(),
        Some(InferenceError::InvalidUpstream)
    );
}

#[tokio::test]
async fn statuses_are_sanitized_and_bodies_never_forwarded() {
    for (status, expected) in [
        (400, InferenceError::UpstreamRejected),
        (401, InferenceError::Configuration),
        (403, InferenceError::Configuration),
        (408, InferenceError::Timeout),
        (413, InferenceError::UpstreamRejected),
        (429, InferenceError::Busy),
        (500, InferenceError::UpstreamUnavailable),
        (307, InferenceError::Configuration),
    ] {
        let mock = Mock::serve(
            "/v1",
            status,
            br#"{"error":{"message":"moderation_blocked: secret prompt text sk-proj-123"}}"#
                .to_vec(),
        )
        .await;
        let error = adapter(&mock)
            .execute_images(&target(), request(1))
            .await
            .err()
            .unwrap();
        assert_eq!(error, expected, "{status}");
        assert!(!error.to_string().contains("secret"));
        assert_eq!(mock.count(), 1, "never retried");
    }
}

#[tokio::test]
async fn unsupported_and_misconfigured_targets_never_reach_the_network() {
    let mock = Mock::json("/v1", body(&[PNG_1X1], "png")).await;
    let a = adapter(&mock);
    let mut dalle = target();
    dalle.upstream_model = "dall-e-3".into();
    let mut seeded = request(1);
    seeded.seed = Some(1);
    let mut tier = request(1);
    tier.size = Some(ImageSize::Tier(ImageTier::K1));
    let mut wide = request(1);
    wide.size = Some(ImageSize::Pixels {
        width: 1792,
        height: 1024,
    });
    assert!(a.supports_image_request(&target(), &request(4)));
    for (t, r) in [
        (dalle, request(1)),
        (target(), seeded),
        (target(), tier),
        (target(), wide),
    ] {
        assert!(!a.supports_image_request(&t, &r));
        assert_eq!(
            a.execute_images(&t, r).await.err(),
            Some(InferenceError::Unsupported)
        );
    }
    let mut endpoint = target();
    endpoint.endpoint = Some("https://evil.example/v1".into());
    let mut region = target();
    region.region = Some("us".into());
    let mut none = target();
    none.credential_ref = "none".into();
    for t in [endpoint, region, none] {
        assert_eq!(
            a.execute_images(&t, request(1)).await.err(),
            Some(InferenceError::Configuration)
        );
    }
    assert_eq!(mock.count(), 0);
    assert!(a.supports_protocol(crate::inference::types::ApiProtocol::Images));
}

#[tokio::test]
async fn dropping_the_request_cancels_upstream() {
    let dropped = Arc::new(tokio::sync::Notify::new());
    let mock = Mock::pending("/v1", dropped.clone()).await;
    let a = adapter(&mock);
    let t = target();
    let mut future = Box::pin(a.execute_images(&t, request(1)));
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
fn usage_normalization() {
    let u = usage(&json!({"input_tokens":20,"input_tokens_details":{"cached_tokens":5,"text_tokens":20,"image_tokens":0},"output_tokens":100})).unwrap();
    let b = u.billing.unwrap();
    assert_eq!(
        (b.uncached_input_tokens, b.cache_read_input_tokens),
        (Some(15), Some(5))
    );
    // Unknown stays unknown, never zero.
    let u = usage(&Value::Null).unwrap();
    assert_eq!(
        (u.input_tokens, u.output_tokens, u.billing),
        (None, None, None)
    );
    let u = usage(&json!({"output_tokens": 3})).unwrap();
    assert_eq!(u.input_tokens, None);
    assert_eq!(u.billing.unwrap().uncached_input_tokens, None);
    for bad in [
        json!({"input_tokens":1,"input_tokens_details":{"cached_tokens":2}}),
        json!({"input_tokens":9,"input_tokens_details":{"text_tokens":8,"image_tokens":0}}),
        json!({"input_tokens":-1}),
        json!({"input_tokens":"9"}),
        json!([]),
    ] {
        assert!(usage(&bad).is_err(), "{bad}");
    }
}
