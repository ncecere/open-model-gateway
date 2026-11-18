//! JSON preflight without building a value tree and small, explicit mapper budgets. Refusing
//! the whole draft avoids presenting a truncated prefix as a conservative rate.
use super::*;
use serde::de::{DeserializeSeed, MapAccess, SeqAccess, Visitor};

pub(super) const MAX_ITEMS: usize = 64;
pub(super) const MAX_WARNINGS: usize = 64;
const MAPPING_NODES: usize = 4096;
const MAPPING_TEXT: usize = 16_384;
const CATALOG_NODES: usize = 131_072;
const CATALOG_TEXT: usize = 16_384;

struct Budget {
    nodes: usize,
    items: usize,
    text: usize,
}
struct Check<'a>(&'a mut Budget);
impl<'de> DeserializeSeed<'de> for Check<'_> {
    type Value = ();
    fn deserialize<D: serde::Deserializer<'de>>(self, d: D) -> Result<(), D::Error> {
        self.0.nodes = self
            .0
            .nodes
            .checked_sub(1)
            .ok_or_else(|| serde::de::Error::custom("catalog complexity limit"))?;
        d.deserialize_any(self)
    }
}
impl<'de> Visitor<'de> for Check<'_> {
    type Value = ();
    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("bounded catalog JSON")
    }
    fn visit_bool<E: serde::de::Error>(self, _: bool) -> Result<(), E> {
        Ok(())
    }
    fn visit_i64<E: serde::de::Error>(self, _: i64) -> Result<(), E> {
        Ok(())
    }
    fn visit_u64<E: serde::de::Error>(self, _: u64) -> Result<(), E> {
        Ok(())
    }
    fn visit_f64<E: serde::de::Error>(self, _: f64) -> Result<(), E> {
        Ok(())
    }
    fn visit_unit<E: serde::de::Error>(self) -> Result<(), E> {
        Ok(())
    }
    fn visit_str<E: serde::de::Error>(self, s: &str) -> Result<(), E> {
        if s.len() > self.0.text {
            return Err(E::custom("catalog text limit"));
        }
        Ok(())
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut a: A) -> Result<(), A::Error> {
        let mut n = 0;
        while a.next_element_seed(Check(self.0))?.is_some() {
            n += 1;
            if n > self.0.items {
                return Err(serde::de::Error::custom("catalog item limit"));
            }
        }
        Ok(())
    }
    fn visit_map<A: MapAccess<'de>>(self, mut a: A) -> Result<(), A::Error> {
        let mut n = 0;
        while let Some(key) = a.next_key::<std::borrow::Cow<'de, str>>()? {
            n += 1;
            if n > self.0.items || key.len() > self.0.text {
                return Err(serde::de::Error::custom("catalog field limit"));
            }
            a.next_value_seed(Check(self.0))?;
        }
        Ok(())
    }
}
fn check_json(body: &[u8], budget: &mut Budget) -> Result<(), ()> {
    if body.len() > BODY_CAP {
        return Err(());
    }
    let mut d = serde_json::Deserializer::from_slice(body);
    Check(budget).deserialize(&mut d).map_err(|_| ())?;
    d.end().map_err(|_| ())
}
pub(super) fn catalog_json(body: &[u8]) -> Result<(), ()> {
    check_json(
        body,
        &mut Budget {
            nodes: CATALOG_NODES,
            items: CATALOG_NODES,
            text: CATALOG_TEXT,
        },
    )
}
pub(super) fn mapping_json(body: &[u8]) -> bool {
    check_json(
        body,
        &mut Budget {
            nodes: MAPPING_NODES,
            items: MAX_ITEMS,
            text: MAPPING_TEXT,
        },
    )
    .is_ok()
}
pub(super) fn mapping_value(v: &Value) -> bool {
    fn walk(v: &Value, remaining: &mut usize, depth: usize) -> bool {
        if *remaining == 0 || depth > 64 {
            return false;
        }
        *remaining -= 1;
        match v {
            Value::String(s) => s.len() <= MAPPING_TEXT,
            Value::Array(a) => {
                a.len() <= MAX_ITEMS && a.iter().all(|v| walk(v, remaining, depth + 1))
            }
            Value::Object(o) => {
                o.len() <= MAX_ITEMS
                    && o.iter()
                        .all(|(k, v)| k.len() <= MAPPING_TEXT && walk(v, remaining, depth + 1))
            }
            _ => true,
        }
    }
    let mut remaining = MAPPING_NODES;
    walk(v, &mut remaining, 0)
}

/// No prefix rates survive a complexity refusal, including request fees and
/// not-applicable assertions. The operator must enter and review all rates.
pub(super) fn refused(model: &Value, workload: WorkloadKind) -> Value {
    // Keep the normal workload ceiling rules without traversing the refused
    // pricing data. Only bounded scalar metadata can reach the mapper here.
    let metadata = json!({
        "id": model["id"].as_str().filter(|s| s.len() <= 200),
        "context_length": model["context_length"].as_u64(),
        "top_provider": {"max_completion_tokens": model["top_provider"]["max_completion_tokens"].as_u64()},
    });
    let mut out = map(&metadata, workload, None);
    let unknown: Vec<_> = Meter::ALL
        .iter()
        .map(|m| json!({"meter":m.as_str(),"unknown":true}))
        .collect();
    out["lines"] = json!(Meter::ALL.iter().map(|m| json!({"meter":m.as_str(),"microusd_per_batch":null,"needs_review":true,"source":"manual"})).collect::<Vec<_>>());
    out["draft"]["price_lines"] = json!(unknown);
    out["display_lines"] = Value::Null;
    out["warnings"] = json!([
        "Catalog pricing exceeds safe draft limits; no prices were imported. Enter and review the rates manually.",
        "The draft lines failed validation; edit them before publishing."
    ]);
    out["needs_review"] = json!(true);
    out
}
