//! The error vocabulary and the four distinct error envelopes (DB-free; the real
//! renderers and the real router over a never-connected lazy pool).
use super::{
    errors as e, generator as g,
    responses::{self as r, validate_documented, validate_value},
    tests::generated,
};
use crate::protocols::batch_line_error;
use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use tower::ServiceExt;

fn schema(name: &str) -> Value {
    json!({"$ref":format!("#/components/schemas/{name}")})
}
fn strings(document: &Value, name: &str) -> BTreeSet<String> {
    document["components"]["schemas"][name]["enum"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn inference_openapi_vocabulary_covers_every_variant_and_renderer() {
    let doc = generated();
    let (codes, types) = (
        strings(&doc, "OpenAiErrorCode"),
        strings(&doc, "OpenAiErrorType"),
    );
    let anthropic_types = strings(&doc, "AnthropicErrorType");
    assert_eq!(e::all().len(), 17 + 2 * 4, "variant list is not exhaustive");
    for error in e::all() {
        let (_, openai) = batch_line_error(false, error);
        let (_, anthropic) = batch_line_error(true, error);
        assert!(codes.contains(error.code()), "{error:?} code missing");
        assert!(types.contains(openai["error"]["type"].as_str().unwrap()));
        assert!(anthropic_types.contains(anthropic["error"]["type"].as_str().unwrap()));
        // The real renderers' output validates against ITS envelope only.
        validate_value(&doc, &schema("OpenAiErrorEnvelope"), &openai).unwrap();
        validate_value(&doc, &schema("AnthropicErrorEnvelope"), &anthropic).unwrap();
        assert!(
            validate_value(&doc, &schema("AnthropicErrorEnvelope"), &openai).is_err(),
            "an OpenAI error validated as an Anthropic error"
        );
        assert!(
            validate_value(&doc, &schema("OpenAiErrorEnvelope"), &anthropic).is_err(),
            "an Anthropic error validated as an OpenAI error"
        );
        // Anthropic's shape has no machine code (coordinator decision 5).
        assert!(anthropic["error"].get("code").is_none());
    }
    // The management envelope (error.code = HTTP status, error.reason) is neither.
    let management = json!({"error":{"code":"400","message":"m","reason":"x"}});
    for name in [
        "OpenAiErrorEnvelope",
        "AnthropicErrorEnvelope",
        "JobErrorEnvelope",
    ] {
        assert!(validate_value(&doc, &schema(name), &management).is_err());
    }
}

#[test]
fn inference_openapi_rejects_a_vocabulary_that_misses_a_variant() {
    let good = generated();
    for (name, member) in [
        ("OpenAiErrorCode", "price_unbounded"),
        ("OpenAiErrorCode", "token_reservation_exceeds_limit"),
        ("OpenAiErrorType", "insufficient_quota"),
        ("AnthropicErrorType", "overloaded_error"),
        ("JobErrorCode", "video_not_ready"),
    ] {
        let mut bad = good.clone();
        let list = bad["components"]["schemas"][name]["enum"]
            .as_array_mut()
            .unwrap();
        let before = list.len();
        list.retain(|v| v != member);
        assert_eq!(list.len() + 1, before, "{name} never listed {member}");
        assert!(g::verify(&bad).is_err(), "accepted {name} without {member}");
    }
    // And an invented member is as wrong as a missing one.
    let mut bad = good.clone();
    bad["components"]["schemas"]["OpenAiErrorCode"]["enum"]
        .as_array_mut()
        .unwrap()
        .push(json!("invented_code"));
    assert!(g::verify(&bad).is_err());
    g::verify(&good).unwrap();
}

#[test]
fn inference_openapi_transport_error_literals_are_still_in_source() {
    for (kind, code, file) in e::OPENAI_TRANSPORT {
        let source =
            std::fs::read_to_string(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(file))
                .unwrap();
        for literal in [kind, code] {
            assert!(
                source.contains(&format!("\"{literal}\"")),
                "{file} no longer renders {literal}: update OPENAI_TRANSPORT"
            );
        }
    }
    for (kind, file) in e::ANTHROPIC_TRANSPORT {
        let source =
            std::fs::read_to_string(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(file))
                .unwrap();
        assert!(source.contains(&format!("\"{kind}\"")), "{file}: {kind}");
    }
}

#[test]
fn inference_openapi_job_codes_are_scanned_from_source() {
    let codes = e::job_codes().unwrap();
    for known in [
        "not_found",
        "video_not_ready",
        "video_in_progress",
        "batch_not_cancellable",
        "invalid_metadata",
    ] {
        assert!(codes.contains(known), "{known}");
    }
    let mut scanned = BTreeSet::new();
    e::scan_source(
        r#"
        fn a() -> JobError { JobError::Conflict("alpha", "m") }
        fn b() -> JobError { crate::jobs::JobError::Forbidden("beta", "m") }
        fn c() -> JobError { JobError::NotFound }
        fn d() -> Other { Other::Conflict("not_a_job", "m") }
        #[cfg(test)]
        mod tests { fn t() -> JobError { JobError::Invalid("only_in_tests", "m") } }
        "#,
        &mut scanned,
    )
    .unwrap();
    assert_eq!(
        scanned,
        BTreeSet::from(["alpha".to_owned(), "beta".to_owned()])
    );
    // An expression that is not a literal fails closed instead of being guessed.
    assert!(
        e::scan_source(
            "fn a(code: &'static str) -> JobError { JobError::Invalid(code, \"m\") }",
            &mut BTreeSet::new()
        )
        .is_err()
    );
}

fn app() -> axum::Router {
    // sqlx retries a refused connection until the acquire timeout: keep it short.
    let pool = sqlx::postgres::PgPoolOptions::new()
        .acquire_timeout(std::time::Duration::from_millis(50))
        .connect_lazy("postgres://unused:unused@127.0.0.1:1/unused")
        .unwrap();
    crate::http::router(crate::store::Store::new(pool))
}
async fn send(request: Request<Body>) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
    let response = app().oneshot(request).await.unwrap();
    let (parts, body) = response.into_parts();
    let bytes = to_bytes(body, 8 * 1024 * 1024).await.unwrap().to_vec();
    (parts.status, parts.headers, bytes)
}
