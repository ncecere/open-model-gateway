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
    // `guard` is exhaustive over InferenceError (a new variant does not compile);
    // `all()` must reach every one of its indices, so the two cannot drift apart.
    let reached: BTreeSet<usize> = e::all().into_iter().map(e::guard).collect();
    assert_eq!(
        reached,
        (0..=20).collect::<BTreeSet<_>>(),
        "all() misses an InferenceError variant that guard() knows"
    );
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
fn inference_openapi_keeps_the_envelopes_distinct() {
    let good = generated();
    // Swapping the two shapes is a drift of the generated envelopes ...
    let mut swapped = good.clone();
    let (openai, anthropic) = (
        good["components"]["schemas"]["OpenAiErrorEnvelope"].clone(),
        good["components"]["schemas"]["AnthropicErrorEnvelope"].clone(),
    );
    swapped["components"]["schemas"]["OpenAiErrorEnvelope"] = anthropic.clone();
    swapped["components"]["schemas"]["AnthropicErrorEnvelope"] = openai;
    assert!(g::verify(&swapped).is_err(), "accepted swapped envelopes");
    // ... and so is an operation that answers in another protocol's shape.
    for (path, method, status, wrong) in [
        ("/v1/messages", "post", "401", "OpenAiErrorEnvelope"),
        ("/v1/messages", "post", "413", "OpenAiErrorEnvelope"),
        ("/v1/embeddings", "post", "400", "AnthropicErrorEnvelope"),
    ] {
        let mut bad = good.clone();
        bad["paths"][path][method]["responses"][status] = json!({
            "description":"wrong family",
            "content":{"application/json":{"schema":schema(wrong)}}});
        assert!(
            g::verify(&bad).is_err(),
            "{path} {status} accepted the {wrong}"
        );
    }
    // The shared responses carry the OpenAI envelope: pointing one at Anthropic fails too.
    let mut bad = good.clone();
    bad["components"]["responses"]["OpenAiUnauthorized"]["content"]["application/json"]["schema"] =
        schema("AnthropicErrorEnvelope");
    assert!(g::verify(&bad).is_err());
    g::verify(&good).unwrap();
}

#[test]
fn inference_openapi_never_reuses_the_management_error_envelope() {
    let good = generated();
    let management =
        crate::management::openapi::responses::artifact()["components"]["schemas"]["ErrorEnvelope"]
            .clone();
    assert!(management["properties"]["error"]["properties"]["reason"].is_object());
    // A component of that name, with the management shape, is refused.
    let mut named = good.clone();
    named["components"]["schemas"]["ErrorEnvelope"] = management.clone();
    assert!(
        g::verify(&named).is_err(),
        "accepted a management ErrorEnvelope"
    );
    // So is an operation that references it.
    let mut referenced = good.clone();
    referenced["paths"]["/v1/embeddings"]["post"]["responses"]["400"] = json!({
        "description":"management envelope",
        "content":{"application/json":{"schema":schema("ErrorEnvelope")}}});
    assert!(g::verify(&referenced).is_err());
    // A fragment cannot smuggle one in either (its name collides with nothing generated,
    // but the document must still carry no ErrorEnvelope).
    let mut fragments = g::load_fragments().unwrap();
    fragments.schemas.insert("ErrorEnvelope".into(), management);
    let smuggled = g::generate(&fragments).unwrap();
    assert!(g::verify(&smuggled).is_err());
    // No inference response schema has the management shape.
    let text = serde_json::to_string(&good).unwrap();
    assert!(!text.contains("#/components/schemas/ErrorEnvelope"));
}

#[test]
fn inference_openapi_documents_every_status_the_renderers_produce() {
    use crate::inference::error::InferenceError as E;
    let doc = generated();
    for (path, validation_status) in [
        ("/v1/chat/completions", 400),
        ("/v1/responses", 400),
        ("/v1/messages", 400),
        ("/v1/embeddings", 400),
        ("/v1/rerank", 400),
        ("/v1/images/generations", 400),
        ("/v1/systemone", 422),
    ] {
        let documented: BTreeSet<u16> = doc["paths"][path]["post"]["responses"]
            .as_object()
            .unwrap()
            .keys()
            .filter_map(|status| status.parse().ok())
            .collect();
        let mut produced = BTreeSet::new();
        for error in e::all() {
            let (mut status, _) = batch_line_error(path == "/v1/messages", error);
            if ![
                "/v1/embeddings",
                "/v1/chat/completions",
                "/v1/responses",
                "/v1/messages",
            ]
            .contains(&path)
            {
                // `workload_error`: unsupported is 400, validation is per route.
                match error {
                    E::Unsupported => status = 400,
                    E::InvalidRequest => status = validation_status,
                    _ => {}
                }
            }
            produced.insert(status);
        }
        // 413 is the body cap, not an InferenceError; 200 the success.
        let mut expected = produced.clone();
        expected.extend([200, 413]);
        if path == "/v1/messages" {
            expected.insert(405);
        }
        assert_eq!(
            documented, expected,
            "{path}: statuses differ from the renderers"
        );
    }
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

/// Real transport refusals of the real router, validated against the document.
#[tokio::test]
async fn inference_openapi_real_transport_errors_match_their_envelopes() {
    let doc = generated();
    // OpenAI shape: no credential.
    let (status, headers, bytes) = send(
        Request::builder()
            .uri("/v1/models")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(headers["www-authenticate"], "Bearer");
    r::validate(&doc, "GET", "/v1/models", status, &headers, &bytes).unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["error"]["code"], "authentication_error");
    // Anthropic shape: no credential on /v1/messages; no machine code.
    let (status, headers, bytes) = send(
        Request::builder()
            .method("POST")
            .uri("/v1/messages")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    validate_documented(&doc, "POST", "/v1/messages", status, &headers, &bytes).unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        (body["type"].as_str(), body["error"].get("code")),
        (Some("error"), None)
    );
    // The same refusal must not validate as an OpenAI error on that path.
    assert!(validate_value(&doc, &schema("OpenAiErrorEnvelope"), &body).is_err());
}

/// The documented exception of /v1/messages: the outer layer answers an
/// oversized body in Anthropic's shape (`request_too_large`), while every other
/// route answers in its own (http.rs request_context, protocols/messages.rs).
#[tokio::test]
async fn inference_openapi_documents_the_messages_transport_exception() {
    let doc = generated();
    let responses = &doc["paths"]["/v1/messages"]["post"]["responses"];
    for status in ["405", "413"] {
        assert_eq!(
            responses[status]["content"]["application/json"]["schema"],
            schema("AnthropicErrorEnvelope"),
            "/v1/messages {status} is not documented in Anthropic's shape"
        );
    }
    assert_eq!(
        responses["405"]["headers"]["allow"]["schema"],
        json!({"const":"POST"})
    );
    // The real 413 (a declared length over the cap, before any credential).
    let too_large = 2 * 1024 * 1024 + 1;
    let (status, headers, bytes) = send(
        Request::builder()
            .method("POST")
            .uri("/v1/messages?beta=true")
            .header("content-length", too_large)
            .body(Body::from(vec![b'x'; too_large]))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    validate_documented(&doc, "POST", "/v1/messages", status, &headers, &bytes).unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["error"]["type"], "request_too_large");
    // The real 405 body (the status rewrite keeps Allow; the stack adds the rest).
    let method_not_allowed = crate::protocols::messages::http_error(StatusCode::METHOD_NOT_ALLOWED);
    let (parts, body) = method_not_allowed.into_parts();
    let bytes = to_bytes(body, 4096).await.unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(parts.status, StatusCode::METHOD_NOT_ALLOWED);
    validate_value(&doc, &schema("AnthropicErrorEnvelope"), &body).unwrap();
    assert_eq!(body["error"]["type"], "invalid_request_error");
    // Every other route answers its 413 in the OpenAI-shaped family, so a
    // Messages-shaped body there would be a contract violation.
    let (status, headers, bytes) = send(
        Request::builder()
            .method("POST")
            .uri("/v1/embeddings")
            .header("content-length", too_large)
            .body(Body::from(vec![b'x'; too_large]))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    r::validate(&doc, "POST", "/v1/embeddings", status, &headers, &bytes).unwrap();
}

/// The Messages 405/413 exception must be IN the document, not only in prose:
/// removing it from the Messages operation is drift.
#[test]
fn inference_openapi_rejects_a_document_without_the_messages_exception() {
    let good = generated();
    for status in ["405", "413"] {
        let mut bad = good.clone();
        bad["paths"]["/v1/messages"]["post"]["responses"]
            .as_object_mut()
            .unwrap()
            .remove(status);
        assert!(
            g::verify(&bad).is_err(),
            "accepted a document without the /v1/messages {status} exception"
        );
    }
}
