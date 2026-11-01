//! Test-only inference contract (#82): no runtime endpoint or production
//! dependency. It reuses the #81 management harness (fragment loader, offline
//! validator, quality rules, coverage and HTTP boundary vectors) through its
//! `pub(crate)` functions; see `generator.rs` for what had to be copied instead.
//! Documentation: `docs/openapi/inference/README.md`.
mod errors;
mod errors_tests;
mod generator;
mod responses;
mod wiring_tests;
mod workload_tests;

#[cfg(all(test, feature = "integration-tests"))]
mod fixtures_tests;

/// Reviewed open-JSON locations of the inference contract, in the format and
/// with the semantics of `management_openapi::quality::ARBITRARY_JSON`: the
/// exact pointer plus the exact `$comment` justification. Everything here is
/// content the gateway deliberately does not model (System One forwards TypeSafe
/// content parts verbatim) or a map whose KEYS are chosen by the caller.
pub(crate) const OPEN_CONTENT: &str = "Open JSON by design: System One forwards content parts verbatim (TypeSafe), so the gateway fixes only the JSON kind here.";
const QUESTIONS: &str = "Question keys are caller chosen names: 1 to 64 questions, each key 1 to 128 bytes without control characters. The key is never sent to the model.";
const OPTIONS: &str = "Option keys are caller chosen: 2 to 255 options, each key 1 to 256 bytes without control characters, each value non-blank text or JSON (or null).";
const ANSWERS: &str =
    "Answer keys are the caller's question keys: exactly one answer per question.";
const PROBABILITIES: &str = "Probability keys are the caller's option keys or score level indices.";
const PROVIDER_LEGEND: &str = "Provider-controlled JSON: parse_answers (inference/workload_types.rs, Score) copies each legend value from the System One answer unvalidated (only the keys are checked against the level indices), so any JSON value, including null, a number or blank text, can be returned.";
const LEGEND: &str = "Legend keys are the caller's score level indices.";
pub(crate) const ARBITRARY_JSON: &[(&str, &str)] = &[
    (
        "/components/schemas/post_v1_systemoneRequest/oneOf/0/properties/state/oneOf/1",
        OPEN_CONTENT,
    ),
    (
        "/components/schemas/post_v1_systemoneRequest/oneOf/1/prefixItems/1/oneOf/1",
        OPEN_CONTENT,
    ),
    (
        "/components/schemas/post_v1_systemoneRequest/oneOf/0/properties/state/oneOf/1/items",
        OPEN_CONTENT,
    ),
    (
        "/components/schemas/post_v1_systemoneRequest/oneOf/1/prefixItems/1/oneOf/1/items",
        OPEN_CONTENT,
    ),
    (
        "/components/schemas/SystemoneNoulQuestion/properties/instructions",
        OPEN_CONTENT,
    ),
    (
        "/components/schemas/SystemoneNoulQuestion/properties/criteria/properties/true",
        OPEN_CONTENT,
    ),
    (
        "/components/schemas/SystemoneNoulQuestion/properties/criteria/properties/false",
        OPEN_CONTENT,
    ),
    (
        "/components/schemas/SystemoneChoiceQuestion/properties/instructions",
        OPEN_CONTENT,
    ),
    (
        "/components/schemas/SystemoneChoiceQuestion/properties/criteria/additionalProperties",
        OPEN_CONTENT,
    ),
    (
        "/components/schemas/SystemoneScoreQuestion/properties/instructions",
        OPEN_CONTENT,
    ),
    (
        "/components/schemas/SystemoneScoreQuestion/properties/criteria/items",
        OPEN_CONTENT,
    ),
    (
        "/components/schemas/SystemoneScoreAnswer/properties/legend/additionalProperties",
        PROVIDER_LEGEND,
    ),
    (
        "/components/schemas/post_v1_systemoneRequest/oneOf/0/properties/questions",
        QUESTIONS,
    ),
    (
        "/components/schemas/post_v1_systemoneRequest/oneOf/1/prefixItems/2",
        QUESTIONS,
    ),
    (
        "/components/schemas/SystemoneChoiceQuestion/properties/criteria",
        OPTIONS,
    ),
    (
        "/components/schemas/post_v1_systemoneResponse/properties/answers",
        ANSWERS,
    ),
    (
        "/components/schemas/SystemoneChoiceAnswer/properties/probabilities",
        PROBABILITIES,
    ),
    (
        "/components/schemas/SystemoneScoreAnswer/properties/probabilities",
        PROBABILITIES,
    ),
    (
        "/components/schemas/SystemoneScoreAnswer/properties/legend",
        LEGEND,
    ),
];

#[cfg(test)]
mod tests {
    use super::{generator as g, responses as r};
    use crate::management::openapi::validation as v;
    use serde_json::{Value, json};

    pub(super) fn generated() -> Value {
        g::generate(&g::load_fragments().unwrap()).unwrap()
    }

    #[test]
    fn inference_openapi_artifact_matches_regeneration() {
        let document = generated();
        g::verify(&document).unwrap();
        v::validate_document_with(&document, &|path| r::completed(r::area(path))).unwrap();
        let bytes = g::encode(&document);
        if std::env::var_os("OMG_REGENERATE_OPENAPI").is_some() {
            assert_eq!(std::env::var("OMG_REGENERATE_OPENAPI").unwrap(), "1");
            assert!(
                std::env::var_os("CI").is_none(),
                "regeneration forbidden in CI"
            );
            std::fs::write(g::artifact_path(), &bytes).unwrap();
        }
        assert_eq!(
            std::fs::read(g::artifact_path()).unwrap(),
            bytes,
            "inference OpenAPI drift: update reviewed fragments, then explicitly regenerate"
        );
    }

    #[test]
    fn inference_openapi_reports_incomplete_schemas() {
        let document = generated();
        let operations = g::operation_count(&document);
        let placeholders = g::placeholder_count(&document);
        eprintln!(
            "inference OpenAPI: {operations} operations; {placeholders} incomplete request/response schemas remaining (#82)"
        );
        // Intentionally not acceptance of #82: later PRs must reduce this to zero.
        assert_eq!(
            operations, 27,
            "review route additions and update this baseline"
        );
        assert_eq!(
            placeholders, 40,
            "review schema completion and update this progress baseline"
        );
    }

    #[test]
    fn inference_openapi_metaschema_and_schema_controls() {
        let good = generated();
        v::validate_document_with(&good, &|path| r::completed(r::area(path))).unwrap();
        for mutation in ["openapi", "responses", "schema"] {
            let mut bad = good.clone();
            match mutation {
                "openapi" => bad["openapi"] = json!("3.0.3"),
                "responses" => bad["paths"]["/v1/models"]["get"]["responses"] = json!(false),
                "schema" => {
                    bad["components"]["schemas"]["OpenAiErrorEnvelope"]["type"] =
                        json!("not-a-type")
                }
                _ => unreachable!(),
            }
            assert!(
                v::validate_document_with(&bad, &|path| r::completed(r::area(path))).is_err(),
                "accepted {mutation} corruption"
            );
        }
    }
}

mod sse;
#[path = "sse_tests.rs"]
mod sse_tests;
