//! Handler-side value rules for the DTO-level request vectors.
//!
//! The request schemas document the rules a handler applies after serde has
//! accepted a body (an email's shape, a role's allowed values, the mapping
//! cross-field constraint). `requests::vectors` compares the schema's verdict
//! with a deserializer's, so a DTO whose handler adds such rules is wrapped in
//! [`Checked`]: it deserializes the REAL DTO, then applies the SAME predicate
//! the handler calls (never a parallel re-implementation). Database-dependent
//! checks (existence, ownership) stay out: the schema does not claim them.
use serde::{Deserialize, Deserializer, de::Error};

/// The non-database acceptance rule a handler applies to a parsed body.
pub(crate) trait Semantic {
    fn accepted(&self) -> bool;
}

/// A DTO that deserializes only when it also satisfies [`Semantic`].
pub(crate) struct Checked<T>(#[allow(dead_code)] pub(crate) T);

impl<'de, T: Deserialize<'de> + Semantic> Deserialize<'de> for Checked<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = T::deserialize(deserializer)?;
        if value.accepted() {
            Ok(Self(value))
        } else {
            Err(D::Error::custom("refused by the handler's value rules"))
        }
    }
}

/// The documented JSON request schema of the operation `method` routes `path`
/// to, resolved like an executed request (the most specific static template).
fn request_schema(method: &str, path: &str) -> &'static serde_json::Value {
    let document = crate::management::openapi::responses::artifact();
    let path = path.split('?').next().unwrap();
    let segments: Vec<&str> = path.split('/').collect();
    let verb = method.to_ascii_lowercase();
    let mut best: Option<(&String, Vec<bool>)> = None;
    for (template, item) in document["paths"].as_object().unwrap() {
        let parts: Vec<&str> = template.split('/').collect();
        if item.get(&verb).is_none()
            || parts.len() != segments.len()
            || !parts
                .iter()
                .zip(&segments)
                .all(|(a, b)| (a.starts_with('{') && a.ends_with('}')) || a == b)
        {
            continue;
        }
        let specificity: Vec<bool> = parts.iter().map(|s| !s.starts_with('{')).collect();
        if best.as_ref().is_none_or(|(_, s)| specificity > *s) {
            best = Some((template, specificity));
        }
    }
    let (template, _) = best.expect("request operation must be documented");
    let schema = &document["paths"][template.as_str()][&verb]["requestBody"]["content"]
        ["application/json"]["schema"];
    assert!(!schema.is_null(), "operation has no JSON request body");
    schema
}

/// Validate an executed request body against its exact routed operation,
/// including manually decoded JSON bodies that have no derived struct DTO.
pub(crate) fn check_request(
    method: &str,
    path: &str,
    body: &serde_json::Value,
) -> anyhow::Result<()> {
    crate::management::openapi::responses::validate_value(
        crate::management::openapi::responses::artifact(),
        request_schema(method, path),
        body,
    )
}

/// Manually decoded objects bind their parser's actual allowlist, not a
/// synthetic DTO that could silently disagree with `Json<Value>` handling.
pub(crate) fn manual_fields(method: &str, template: &str, fields: &[&str]) {
    let document = crate::management::openapi::responses::artifact();
    let mut schema = &document["paths"][template][method.to_ascii_lowercase()]["requestBody"]
        ["content"]["application/json"]["schema"];
    while let Some(reference) = schema["$ref"].as_str() {
        schema = document.pointer(&reference[1..]).unwrap();
    }
    let documented: std::collections::BTreeSet<&str> = schema["properties"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        documented,
        fields.iter().copied().collect(),
        "{method} {template}: manual parser/schema field inventory drift"
    );
}
