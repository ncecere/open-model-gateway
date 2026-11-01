//! Request/response contracts of the simple JSON workloads against the REAL
//! serde DTOs and the REAL handler functions (DB-free: a request the handler
//! refuses never reaches the database).
use super::{
    responses::{self as r, embedding_encoding_matches, validate_body},
    tests::generated,
};
use crate::{
    auth::Principal,
    http::RequestId,
    inference::{Engine, EngineLimits},
    management::openapi::requests::vectors_in,
    protocols::{embeddings, images, rerank, systemone},
    providers::ProviderRegistry,
    store::Store,
};
use axum::{
    Extension, Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode},
    routing::post,
};
use serde_json::{Value, json};
use std::sync::Arc;
use tower::ServiceExt;

pub(super) fn embeddings_valid() -> Value {
    json!({"model":"company/smart","input":"hello","encoding_format":"float","dimensions":2})
}
pub(super) fn rerank_valid() -> Value {
    json!({"model":"company/smart","query":"cat","documents":["kitten","airplane","dog"],"top_n":2})
}
pub(super) fn images_valid() -> Value {
    json!({"model":"company/smart","prompt":"a red dot","n":2,"size":"1024x1024","quality":"low","response_format":"b64_json","seed":7})
}
pub(super) fn systemone_valid() -> Value {
    json!({"model":"company/smart","state":"hello there","questions":{
        "is_q":{"type":"noul","instructions":"Is the text a greeting?","criteria":{"true":"yes","false":"no"}},
        "lang":{"type":"choice","instructions":"Language?","criteria":{"en":"English","fr":"French","x":null}},
        "tone":{"type":"score","instructions":"Tone","criteria":["casual",{"text":"neutral"},["formal"]]}}})
}

/// Every field of these DTOs may be stricter in the contract than in serde
/// (lengths, ranges, alphabets, kinds): each is a reviewed refinement the HTTP
/// boundary vectors then compare with the real handler exactly.
const EMBEDDINGS: &[&str] = &["/model", "/input", "/dimensions"];
const RERANK: &[&str] = &["/model", "/query", "/documents", "/top_n"];
const IMAGES: &[&str] = &[
    "/model",
    "/prompt",
    "/n",
    "/size",
    "/quality",
    "/response_format",
    "/seed",
];
const SYSTEMONE: &[&str] = &["/model", "/state", "/questions"];

#[test]
fn inference_openapi_request_schemas_match_the_real_serde_dtos() {
    let doc = generated();
    vectors_in::<embeddings::Request>(
        &doc,
        "POST",
        "/v1/embeddings",
        embeddings_valid(),
        EMBEDDINGS,
    )
    .unwrap_or_else(|e| panic!("{e:#}"));
    vectors_in::<rerank::Request>(&doc, "POST", "/v1/rerank", rerank_valid(), RERANK)
        .unwrap_or_else(|e| panic!("{e:#}"));
    vectors_in::<images::Request>(
        &doc,
        "POST",
        "/v1/images/generations",
        images_valid(),
        IMAGES,
    )
    .unwrap_or_else(|e| panic!("{e:#}"));
    vectors_in::<systemone::Request>(&doc, "POST", "/v1/systemone", systemone_valid(), SYSTEMONE)
        .unwrap_or_else(|e| panic!("{e:#}"));
}

#[test]
fn inference_openapi_rejects_an_invented_request_field() {
    let good = generated();
    // The embeddings DTO is `deny_unknown_fields` with four fields; a schema that
    // documents a fifth (OpenAI's `user`, say) promises what the handler refuses.
    let mut invented = good.clone();
    invented["components"]["schemas"]["post_v1_embeddingsRequest"]["oneOf"][0]["properties"]["user"] =
        json!({"type":"string"});
    let error = vectors_in::<embeddings::Request>(
        &invented,
        "POST",
        "/v1/embeddings",
        embeddings_valid(),
        EMBEDDINGS,
    )
    .expect_err("accepted a field the DTO does not have");
    assert!(
        format!("{error:#}").contains("field inventory drift"),
        "{error:#}"
    );
    // The same for a field the DTO has but the schema omits.
    let mut dropped = good.clone();
    dropped["components"]["schemas"]["post_v1_embeddingsRequest"]["oneOf"][0]["properties"]
        .as_object_mut()
        .unwrap()
        .remove("dimensions");
    assert!(
        vectors_in::<embeddings::Request>(
            &dropped,
            "POST",
            "/v1/embeddings",
            embeddings_valid(),
            EMBEDDINGS,
        )
        .is_err()
    );
    // A schema looser than serde (an accepted `input` of any type) is never tolerated.
    let mut loose = good.clone();
    loose["components"]["schemas"]["post_v1_embeddingsRequest"]["oneOf"][0]["properties"]["input"] =
        json!({"type":["string","array","integer"]});
    assert!(
        vectors_in::<embeddings::Request>(
            &loose,
            "POST",
            "/v1/embeddings",
            embeddings_valid(),
            EMBEDDINGS,
        )
        .is_err()
    );
    vectors_in::<embeddings::Request>(
        &good,
        "POST",
        "/v1/embeddings",
        embeddings_valid(),
        EMBEDDINGS,
    )
    .unwrap();
}

/// The embedding representation follows the request's `encoding_format`: the
/// schema allows both at every vector, so a base64 request answered with float
/// arrays (or the reverse) is caught here.
#[test]
fn inference_openapi_embedding_vectors_follow_the_requested_encoding() {
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    let bytes: Vec<u8> = [1.0f32, -2.0, 0.5]
        .iter()
        .flat_map(|f| f.to_le_bytes())
        .collect();
    let floats = json!({"object":"list","model":"m","data":[
        {"object":"embedding","index":0,"embedding":[1.0,-2.0,0.5]}]});
    let base64 = json!({"object":"list","model":"m","data":[
        {"object":"embedding","index":0,"embedding":STANDARD.encode(&bytes)}]});
    let float_request = json!({"model":"m","input":"x","encoding_format":"float"});
    let default_request = json!({"model":"m","input":"x"});
    let null_request = json!({"model":"m","input":"x","encoding_format":null});
    let base64_request = json!({"model":"m","input":"x","encoding_format":"base64"});
    let positional = json!(["m", "x", "base64", null]);
    for request in [&float_request, &default_request, &null_request] {
        embedding_encoding_matches(request, &floats).unwrap();
        assert!(embedding_encoding_matches(request, &base64).is_err());
    }
    embedding_encoding_matches(&base64_request, &base64).unwrap();
    embedding_encoding_matches(&positional, &base64).unwrap();
    for request in [&base64_request, &positional] {
        assert!(
            embedding_encoding_matches(request, &floats).is_err(),
            "float arrays answered a base64 request"
        );
    }
    // Representation details: whole float32 values, one vector per input, one length.
    let truncated = json!({"data":[{"index":0,"embedding":STANDARD.encode([0u8; 6])}]});
    assert!(embedding_encoding_matches(&base64_request, &truncated).is_err());
    let two_inputs = json!({"model":"m","input":["a","b"],"encoding_format":"base64"});
    assert!(embedding_encoding_matches(&two_inputs, &base64).is_err());
    // The schema itself documents both representations (and nothing else).
    let doc = generated();
    let item = json!({"$ref":"#/components/schemas/EmbeddingItem"});
    for embedding in [
        json!([1.0, 2.0]),
        json!("AAAAAA=="),
        json!(null),
        json!({}),
        json!(7),
    ] {
        let valid = embedding.is_array() || embedding.is_string();
        let result = r::validate_value(
            &doc,
            &item,
            &json!({"object":"embedding","index":0,"embedding":embedding}),
        );
        assert_eq!(result.is_ok(), valid);
    }
}

/// A score answer's legend values come from the provider unvalidated
/// (`parse_answers`, Score: only the keys are checked), so the response schema
/// must accept any JSON value there; the keys stay the level indices.
#[test]
fn inference_openapi_score_legend_values_are_provider_json() {
    let doc = generated();
    let answer = json!({"$ref":"#/components/schemas/SystemoneScoreAnswer"});
    for legend in [
        json!({"0":"casual","1":"formal"}),
        json!({"0":null,"1":5}),
        json!({"0":"  ","1":true}),
        json!({"0":[{"type":"text","text":"a"}],"1":{"k":1}}),
    ] {
        let body =
            json!({"type":"score","score":1,"legend":legend,"probabilities":{"0":0.5,"1":0.5}});
        assert!(r::validate_value(&doc, &answer, &body).is_ok());
    }
}

/// The real handlers, served below the transport layer (their extractors, serde
/// and error mapping are the production code; only authentication and the
/// outer layers are replaced by extensions), refuse an invalid body BEFORE any
/// storage access.
fn handlers() -> Router {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .acquire_timeout(std::time::Duration::from_millis(50))
        .connect_lazy("postgres://unused:unused@127.0.0.1:1/unused")
        .unwrap();
    let store = Store::new(pool);
    let engine = Engine::new(
        Arc::new(store),
        ProviderRegistry::default(),
        EngineLimits::default(),
    )
    .unwrap();
    Router::new()
        .route("/v1/embeddings", post(embeddings::handle))
        .route("/v1/rerank", post(rerank::handle))
        .route("/v1/images/generations", post(images::handle))
        .route("/v1/systemone", post(systemone::handle))
        .layer(Extension(engine))
        .layer(Extension(Principal {
            key_id: uuid::Uuid::nil(),
            workspace_id: uuid::Uuid::nil(),
            user_id: None,
        }))
        .layer(Extension(RequestId(uuid::Uuid::now_v7())))
}
async fn refusal(path: &str, body: &Value) -> (StatusCode, Vec<u8>) {
    let response = handlers()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(path)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    (
        status,
        to_bytes(response.into_body(), 65536)
            .await
            .unwrap()
            .to_vec(),
    )
}

/// TypeSafe's System One answers a validation failure with 422; every other
/// workload with 400 (embeddings too). The statuses come from the real
/// handlers and must be exactly the documented ones.
#[tokio::test]
async fn inference_openapi_validation_status_is_422_for_systemone_and_400_elsewhere() {
    let good = generated();
    let cases = [
        ("/v1/embeddings", json!({"model":"m","input":[]}), 400),
        (
            "/v1/rerank",
            json!({"model":"m","query":"q","documents":[]}),
            400,
        ),
        (
            "/v1/images/generations",
            json!({"model":"m","prompt":""}),
            400,
        ),
        (
            "/v1/systemone",
            json!({"model":"m","state":"x","questions":{}}),
            422,
        ),
        (
            "/v1/systemone",
            json!({"model":"m","state":"x","extra":1,"questions":{}}),
            422,
        ),
        ("/v1/systemone", json!("not an object"), 422),
    ];
    for (path, body, expected) in &cases {
        let (status, bytes) = refusal(path, body).await;
        assert_eq!(status.as_u16(), *expected, "{path} validation status");
        validate_body(&good, "POST", path, status, &bytes).unwrap();
        let parsed: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(parsed["error"]["code"], "invalid_request_error");
        // A contract that documents the other status for this path is wrong.
        let other = if *expected == 422 { "400" } else { "422" };
        let mut bad = good.clone();
        let responses = bad["paths"][*path]["post"]["responses"]
            .as_object_mut()
            .unwrap();
        let entry = responses.remove(&expected.to_string()).unwrap();
        responses.insert(other.to_owned(), entry);
        assert!(
            validate_body(&bad, "POST", path, status, &bytes).is_err(),
            "{path}: documenting {other} instead of {expected} was accepted"
        );
    }
    // And the contract names them: 422 only for System One.
    for (path, has_422) in [
        ("/v1/systemone", true),
        ("/v1/embeddings", false),
        ("/v1/rerank", false),
        ("/v1/images/generations", false),
    ] {
        assert_eq!(
            good["paths"][path]["post"]["responses"]
                .get("422")
                .is_some(),
            has_422,
            "{path}"
        );
    }
}

/// The documented unsupported-capability statuses of the handlers that can
/// produce one without a database: images refuse `response_format: "url"` with
/// 400 `unsupported_capability` (not 501), embeddings keep 501 elsewhere.
#[tokio::test]
async fn inference_openapi_unsupported_images_url_is_a_400_refusal() {
    let good = generated();
    let (status, bytes) = refusal(
        "/v1/images/generations",
        &json!({"model":"m","prompt":"p","response_format":"url"}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    validate_body(&good, "POST", "/v1/images/generations", status, &bytes).unwrap();
    let parsed: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(parsed["error"]["code"], "unsupported_capability");
}
