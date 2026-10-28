//! Anthropic Cost Report v1. No floating point and no provider connectivity.
use super::*;
use chrono::{DateTime, Duration, Utc};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;

pub(super) const MAX_BYTES: usize = 5 * 1024 * 1024;
const MAX_LINES: usize = 50_000;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Upload {
    pub account_id: String,
    pub evidence_reference: String,
    pub exported_at: String,
    pub supersedes: Option<Uuid>,
    pub request: Parameters,
    pub pages: Vec<Box<serde_json::value::RawValue>>,
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Parameters {
    pub starting_at: String,
    pub ending_at: String,
    bucket_width: String,
    group_by: Vec<String>,
    limit: u32,
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Page {
    data: Vec<Bucket>,
    has_more: bool,
    #[serde(deserialize_with = "nullable")]
    next_page: Option<String>,
}
fn nullable<'de, D: serde::Deserializer<'de>, T: Deserialize<'de>>(
    d: D,
) -> Result<Option<T>, D::Error> {
    Option::deserialize(d)
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Bucket {
    starting_at: String,
    ending_at: String,
    results: Vec<Item>,
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Item {
    amount: String,
    currency: String,
    workspace_id: Option<String>,
    description: Option<String>,
    model: Option<String>,
    service_tier: Option<String>,
    context_window: Option<String>,
    cost_type: Option<String>,
    token_type: Option<String>,
    inference_geo: Option<String>,
    speed: Option<String>,
}
#[derive(Serialize)]
pub(super) struct Line {
    pub source_page: i32,
    pub source_pointer: String,
    pub bucket_start: DateTime<Utc>,
    pub bucket_end: DateTime<Utc>,
    pub dimensions: Value,
    pub original_amount: String,
    pub amount_microusd: String,
    pub conversion_residual_microusd: String,
    pub dimension_key: String,
}
pub(super) struct Parsed {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub exported: DateTime<Utc>,
    pub fingerprint: Vec<u8>,
    pub lines: Vec<Line>,
}
pub(super) fn bounded(s: &str, max: usize) -> bool {
    !s.is_empty() && s.len() <= max && !s.chars().any(char::is_control)
}
pub(super) fn timestamp(s: &str) -> Result<DateTime<Utc>, ApiError> {
    if s.len() > 40 {
        return Err(invalid());
    }
    DateTime::parse_from_rfc3339(s)
        .map(|d| d.with_timezone(&Utc))
        .map_err(|_| invalid())
}

/// Exact decimal cents -> rounded integer micro-USD and exact signed residual
/// (unrounded micro-USD minus rounded micro-USD). Precision is bounded, never truncated.
pub(crate) fn cents(s: &str) -> Result<(i128, String), ApiError> {
    if s.is_empty() || s.len() > 64 {
        return Err(invalid());
    }
    let negative = s.starts_with('-');
    let unsigned = if negative { &s[1..] } else { s };
    let (whole, fraction) = unsigned.split_once('.').unwrap_or((unsigned, ""));
    if whole.is_empty()
        || !whole.bytes().all(|b| b.is_ascii_digit())
        || (unsigned.contains('.') && fraction.is_empty())
        || fraction.len() > 18
        || !fraction.bytes().all(|b| b.is_ascii_digit())
    {
        return Err(invalid());
    }
    let base: i128 = whole.parse().map_err(|_| invalid())?;
    let padded = format!("{fraction:0<4}");
    let first: i128 = padded[..4].parse().map_err(|_| invalid())?;
    let mut amount = base
        .checked_mul(10_000)
        .and_then(|n| n.checked_add(first))
        .ok_or_else(invalid)?;
    let rest = fraction.get(4..).unwrap_or("");
    let numerator: i128 = if rest.is_empty() {
        0
    } else {
        rest.parse().map_err(|_| invalid())?
    };
    let denominator = 10_i128.pow(rest.len() as u32);
    let up = numerator * 2 > denominator || (numerator * 2 == denominator && amount % 2 != 0);
    if up {
        amount = amount.checked_add(1).ok_or_else(invalid)?;
    }
    let mut residual = numerator - if up { denominator } else { 0 };
    if negative {
        amount = -amount;
        residual = -residual;
    }
    let residual = if residual == 0 {
        "0".into()
    } else {
        let digits = format!("{:0width$}", residual.abs(), width = rest.len());
        format!(
            "{}0.{}",
            if residual < 0 { "-" } else { "" },
            digits.trim_end_matches('0')
        )
    };
    Ok((amount, residual))
}

/// Bound the raw page before typed deserialization. Strings include JSON keys;
/// decoded strings are checked again below. Fixed typed shapes reject unknown
/// fields and duplicate keys (including repeated null-valued fields).
fn bounded_page(raw: &str) -> Result<(), ApiError> {
    let (mut depth, mut in_string, mut escaped, mut length) = (0_u32, false, false, 0_usize);
    for b in raw.bytes() {
        if in_string {
            length += 1;
            if length > 24_576 {
                return Err(invalid());
            }
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_string = false;
            }
        } else {
            match b {
                b'"' => {
                    in_string = true;
                    length = 0;
                }
                b'[' | b'{' => {
                    depth += 1;
                    if depth > 16 {
                        return Err(invalid());
                    }
                }
                b']' | b'}' => {
                    depth = depth.checked_sub(1).ok_or_else(invalid)?;
                }
                _ => {}
            }
        }
    }
    Ok(())
}

#[cfg(test)]
#[test]
fn management_openapi_money_raw_upload_request_vectors() {
    // The real ingestion decoder is two-stage: Upload preserves RawValue,
    // then parse deserializes each Page without losing duplicate object keys.
    // Exercise that exact typed decoder, not Value-normalized fake input.
    struct DecodedUpload;
    impl<'de> Deserialize<'de> for DecodedUpload {
        fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
            let upload = Upload::deserialize(d)?;
            for raw in upload.pages {
                serde_json::from_str::<Page>(raw.get()).map_err(serde::de::Error::custom)?;
            }
            Ok(Self)
        }
    }
    let upload = json!({"account_id":"account","evidence_reference":"receipt:upload","exported_at":"2025-08-03T00:00:00Z","supersedes":null,"request":{"starting_at":"2025-08-01T00:00:00Z","ending_at":"2025-08-02T00:00:00Z","bucket_width":"1d","group_by":["workspace_id","description"],"limit":31},"pages":[{"data":[{"starting_at":"2025-08-01T00:00:00Z","ending_at":"2025-08-02T00:00:00Z","results":[{"amount":"-1","currency":"USD","workspace_id":null,"description":null,"model":null,"service_tier":null,"context_window":null,"cost_type":null,"token_type":null,"inference_geo":null,"speed":null}]}],"has_more":false,"next_page":null}]});
    super::super::openapi::requests::component_vectors::<Parameters>(
        "MoneyProviderParameters",
        upload["request"].clone(),
    );
    super::super::openapi::requests::component_vectors::<Page>(
        "MoneyProviderPage",
        upload["pages"][0].clone(),
    );
    super::super::openapi::requests::component_vectors::<Bucket>(
        "MoneyProviderBucket",
        upload["pages"][0]["data"][0].clone(),
    );
    super::super::openapi::requests::component_vectors::<Item>(
        "MoneyProviderItem",
        upload["pages"][0]["data"][0]["results"][0].clone(),
    );
    super::super::openapi::requests::vectors::<DecodedUpload>(
        "POST",
        "/api/v1/platform/provider-cost-imports",
        upload,
    );
}
pub(super) fn parse(b: &Upload) -> Result<Parsed, ApiError> {
    let groups: BTreeSet<&str> = b.request.group_by.iter().map(String::as_str).collect();
    if !bounded(&b.account_id, 200)
        || !bounded(&b.evidence_reference, 256)
        || !(1..=31).contains(&b.pages.len())
        || b.request.bucket_width != "1d"
        || !(1..=31).contains(&b.request.limit)
        || b.request.group_by.len() != 2
        || groups != BTreeSet::from(["description", "workspace_id"])
    {
        return Err(invalid());
    }
    let start = timestamp(&b.request.starting_at)?;
    let end = timestamp(&b.request.ending_at)?;
    let exported = timestamp(&b.exported_at)?;
    if start.timestamp().rem_euclid(86_400) != 0
        || start.timestamp_subsec_nanos() != 0
        || end <= start
        || end > Utc::now()
        || end - start > Duration::days(31)
        || end.timestamp().rem_euclid(86_400) != 0
        || end.timestamp_subsec_nanos() != 0
        || exported < end
        || exported > Utc::now()
    {
        return Err(invalid());
    }
    let mut expected = start;
    let mut seen_rows = BTreeSet::new();
    let mut cursors = BTreeSet::new();
    let mut lines = Vec::new();
    let mut canonical = Vec::new();
    for (page_index, raw) in b.pages.iter().enumerate() {
        bounded_page(raw.get())?;
        let page: Page = serde_json::from_str(raw.get()).map_err(|_| invalid())?;
        let last = page_index + 1 == b.pages.len();
        if page.has_more == last
            || page.data.is_empty()
            || page.data.len() > b.request.limit as usize
            || (!last
                && !page
                    .next_page
                    .as_ref()
                    .is_some_and(|s| bounded(s, 4096) && cursors.insert(s.clone())))
            || (last && page.next_page.is_some())
        {
            return Err(invalid());
        }
        for (bucket_index, bucket) in page.data.iter().enumerate() {
            let bs = timestamp(&bucket.starting_at)?;
            let be = timestamp(&bucket.ending_at)?;
            if bs != expected || be != bs + Duration::days(1) || be > end {
                return Err(invalid());
            }
            expected = be;
            for (item_index, item) in bucket.results.iter().enumerate() {
                if lines.len() == MAX_LINES || item.currency != "USD" {
                    return Err(invalid());
                }
                for text in [
                    &item.workspace_id,
                    &item.description,
                    &item.model,
                    &item.service_tier,
                    &item.context_window,
                    &item.cost_type,
                    &item.token_type,
                    &item.inference_geo,
                    &item.speed,
                ]
                .into_iter()
                .flatten()
                {
                    if !bounded(text, 4096) {
                        return Err(invalid());
                    }
                }
                let (amount, residual) = cents(&item.amount)?;
                let mut dimensions = serde_json::to_value(item).map_err(|_| invalid())?;
                dimensions
                    .as_object_mut()
                    .ok_or_else(invalid)?
                    .remove("amount");
                let key =
                    serde_json::to_string(&json!([bs, be, dimensions])).map_err(|_| invalid())?;
                if !seen_rows.insert(key.clone()) {
                    return Err(invalid());
                }
                lines.push(Line {
                    source_page: page_index as i32,
                    source_pointer: format!("/data/{bucket_index}/results/{item_index}"),
                    bucket_start: bs,
                    bucket_end: be,
                    dimensions,
                    original_amount: item.amount.clone(),
                    amount_microusd: amount.to_string(),
                    conversion_residual_microusd: residual,
                    dimension_key: hex::encode(Sha256::digest(key.as_bytes())),
                });
            }
        }
        for bucket in page.data {
            let mut items: Vec<Value> = bucket
                .results
                .iter()
                .map(serde_json::to_value)
                .collect::<Result<_, _>>()
                .map_err(|_| invalid())?;
            items.sort_by_cached_key(Value::to_string);
            canonical.push(json!({"starting_at":timestamp(&bucket.starting_at)?,"ending_at":timestamp(&bucket.ending_at)?,"results":items}));
        }
    }
    if expected != end {
        return Err(invalid());
    }
    // Pagination boundaries, evidence references and export timestamps are not
    // financial identity. Preserve them in evidence, not in the fingerprint.
    let report = serde_json::to_vec(&json!(["anthropic-cost-v1", start, end, canonical]))
        .map_err(|_| invalid())?;
    Ok(Parsed {
        start,
        end,
        exported,
        fingerprint: Sha256::digest(&report).to_vec(),
        lines,
    })
}
