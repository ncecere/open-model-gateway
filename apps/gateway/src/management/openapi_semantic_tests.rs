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
