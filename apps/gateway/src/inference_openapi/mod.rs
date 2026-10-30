//! Test-only inference contract (#82): no runtime endpoint or production
//! dependency. It reuses the #81 management harness (fragment loader, offline
//! validator, quality rules, coverage and HTTP boundary vectors) through its
//! `pub(crate)` functions; see `generator.rs` for what had to be copied instead.
//! Documentation: `docs/openapi/inference/README.md`.
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
        OPEN_CONTENT,
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
