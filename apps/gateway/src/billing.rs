//! Provider-neutral, presence-preserving token metering and exact valuation.
//! Aggregate writes overlap their allocations: only disjoint categories are charged.
use serde::{Deserialize, Serialize};
pub mod v3;

mod counter {
    use serde::{Deserialize, Deserializer, Serializer, de::Error};
    pub fn serialize<S: Serializer>(n: &Option<u64>, s: S) -> Result<S::Ok, S::Error> {
        match n {
            Some(n) => s.serialize_some(&n.to_string()),
            None => s.serialize_none(),
        }
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<u64>, D::Error> {
        Option::<String>::deserialize(d)?
            .map(|s| {
                if s.is_empty() || s.len() > 19 || !s.bytes().all(|b| b.is_ascii_digit()) {
                    return Err(D::Error::custom("invalid token counter"));
                }
                let n: u64 = s
                    .parse()
                    .map_err(|_| D::Error::custom("invalid token counter"))?;
                if n > i64::MAX as u64 {
                    return Err(D::Error::custom("token counter overflow"));
                }
                Ok(n)
            })
            .transpose()
    }
}
mod money {
    use serde::{Deserialize, Deserializer, Serializer, de::Error};
    pub fn serialize<S: Serializer>(n: &i64, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&n.to_string())
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<i64, D::Error> {
        let s = String::deserialize(d)?;
        if s.is_empty() || s.len() > 19 || !s.bytes().all(|b| b.is_ascii_digit()) {
            return Err(D::Error::custom("invalid micro-USD"));
        }
        s.parse()
            .map_err(|_| D::Error::custom("micro-USD overflow"))
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BillingUsage {
    #[serde(with = "counter")]
    pub total_input_tokens: Option<u64>,
    #[serde(with = "counter")]
    pub uncached_input_tokens: Option<u64>,
    #[serde(with = "counter")]
    pub cache_read_input_tokens: Option<u64>,
    #[serde(with = "counter")]
    pub cache_write_input_tokens: Option<u64>,
    #[serde(with = "counter")]
    pub cache_write_default_input_tokens: Option<u64>,
    #[serde(with = "counter")]
    pub cache_write_5m_input_tokens: Option<u64>,
    #[serde(with = "counter")]
    pub cache_write_1h_input_tokens: Option<u64>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BillingError {
    InvalidUsage,
    InvalidRate,
    Overflow,
}
impl std::fmt::Display for BillingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}",
            match self {
                Self::InvalidUsage => "inconsistent billing usage",
                Self::InvalidRate => "invalid rate",
                Self::Overflow => "accounting overflow",
            }
        )
    }
}
impl std::error::Error for BillingError {}
impl BillingUsage {
    pub fn counts(&self) -> [Option<u64>; 7] {
        [
            self.total_input_tokens,
            self.uncached_input_tokens,
            self.cache_read_input_tokens,
            self.cache_write_input_tokens,
            self.cache_write_default_input_tokens,
            self.cache_write_5m_input_tokens,
            self.cache_write_1h_input_tokens,
        ]
    }
    pub fn write_parts(&self) -> [Option<u64>; 3] {
        [
            self.cache_write_default_input_tokens,
            self.cache_write_5m_input_tokens,
            self.cache_write_1h_input_tokens,
        ]
    }
    pub fn validate(&self) -> Result<(), BillingError> {
        if self.counts().iter().flatten().any(|n| *n > i64::MAX as u64) {
            return Err(BillingError::Overflow);
        }
        let writes = self
            .write_parts()
            .into_iter()
            .flatten()
            .map(u128::from)
            .sum::<u128>();
        if writes > i64::MAX as u128 {
            return Err(BillingError::Overflow);
        }
        if self.cache_write_input_tokens.is_some_and(|n| {
            writes > u128::from(n)
                || (self.write_parts().iter().all(Option::is_some) && writes != u128::from(n))
        }) {
            return Err(BillingError::InvalidUsage);
        }
        let input = u128::from(self.uncached_input_tokens.unwrap_or(0))
            + u128::from(self.cache_read_input_tokens.unwrap_or(0))
            + writes.max(u128::from(self.cache_write_input_tokens.unwrap_or(0)));
        if input > i64::MAX as u128 {
            return Err(BillingError::Overflow);
        }
        if self.total_input_tokens.is_some_and(|n| {
            input > u128::from(n)
                || ([
                    self.uncached_input_tokens,
                    self.cache_read_input_tokens,
                    self.cache_write_input_tokens,
                ]
                .iter()
                .all(Option::is_some)
                    && input != u128::from(n))
        }) {
            return Err(BillingError::InvalidUsage);
        }
        Ok(())
    }
    pub fn is_complete(&self) -> bool {
        self.validate().is_ok() && self.counts().iter().all(Option::is_some)
    }
    pub fn input_lower_bound(&self) -> Result<u64, BillingError> {
        self.validate()?;
        let writes = self
            .write_parts()
            .into_iter()
            .flatten()
            .sum::<u64>()
            .max(self.cache_write_input_tokens.unwrap_or(0));
        Ok((self.uncached_input_tokens.unwrap_or(0)
            + self.cache_read_input_tokens.unwrap_or(0)
            + writes)
            .max(self.total_input_tokens.unwrap_or(0)))
    }
    pub fn preserves(&self, old: &Self) -> bool {
        self.validate().is_ok()
            && old
                .counts()
                .into_iter()
                .zip(self.counts())
                .all(|(old, new)| old.is_none_or(|old| new.is_some_and(|new| new >= old)))
    }
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum CacheRate {
    Priced {
        #[serde(with = "money")]
        microusd_per_million: i64,
    },
    #[default]
    Unknown,
    NotApplicable,
}
impl<'de> Deserialize<'de> for CacheRate {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        // Empty struct variants reject accidental amounts; unit variants don't.
        #[derive(Deserialize)]
        #[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
        enum Strict {
            Priced {
                #[serde(with = "money")]
                microusd_per_million: i64,
            },
            Unknown {},
            NotApplicable {},
        }
        Ok(match Strict::deserialize(d)? {
            Strict::Priced {
                microusd_per_million,
            } => Self::Priced {
                microusd_per_million,
            },
            Strict::Unknown {} => Self::Unknown,
            Strict::NotApplicable {} => Self::NotApplicable,
        })
    }
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CachePricing {
    pub read: CacheRate,
    pub write: CacheRate,
    pub write_5m: CacheRate,
    pub write_1h: CacheRate,
}
impl CachePricing {
    pub fn rates(&self) -> [CacheRate; 4] {
        [self.read, self.write, self.write_5m, self.write_1h]
    }
    pub fn validate(&self) -> Result<(), BillingError> {
        if self.rates().iter().any(|r| matches!(r, CacheRate::Priced { microusd_per_million } if *microusd_per_million < 0)) { Err(BillingError::InvalidRate) } else { Ok(()) }
    }
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CostComponents {
    #[serde(with = "money")]
    pub uncached_input_microusd: i64,
    #[serde(with = "money")]
    pub cache_read_microusd: i64,
    #[serde(with = "money")]
    pub cache_write_default_microusd: i64,
    #[serde(with = "money")]
    pub cache_write_5m_microusd: i64,
    #[serde(with = "money")]
    pub cache_write_1h_microusd: i64,
    #[serde(with = "money")]
    pub output_microusd: i64,
}
impl CostComponents {
    pub fn total(&self) -> Result<i64, BillingError> {
        checked_total(self.values())
    }
    pub fn values(&self) -> [i64; 6] {
        [
            self.uncached_input_microusd,
            self.cache_read_microusd,
            self.cache_write_default_microusd,
            self.cache_write_5m_microusd,
            self.cache_write_1h_microusd,
            self.output_microusd,
        ]
    }
    fn from_values(v: [i64; 6]) -> Self {
        Self {
            uncached_input_microusd: v[0],
            cache_read_microusd: v[1],
            cache_write_default_microusd: v[2],
            cache_write_5m_microusd: v[3],
            cache_write_1h_microusd: v[4],
            output_microusd: v[5],
        }
    }
}
/// Non-token meter observations. `None` is unknown, never zero; every key is
/// required in JSON (explicit null), mirroring `BillingUsage`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MeterUsage {
    #[serde(with = "counter")]
    pub output_images: Option<u64>,
    #[serde(with = "counter")]
    pub input_characters: Option<u64>,
    #[serde(with = "counter")]
    pub input_audio_seconds_ms: Option<u64>,
    #[serde(with = "counter")]
    pub output_audio_seconds_ms: Option<u64>,
    #[serde(with = "counter")]
    pub search_units: Option<u64>,
    #[serde(with = "counter")]
    pub requests: Option<u64>,
}
impl MeterUsage {
    pub const KEYS: [&'static str; 6] = [
        "output_images",
        "input_characters",
        "input_audio_seconds_ms",
        "output_audio_seconds_ms",
        "search_units",
        "requests",
    ];
    pub fn counts(&self) -> [Option<u64>; 6] {
        [
            self.output_images,
            self.input_characters,
            self.input_audio_seconds_ms,
            self.output_audio_seconds_ms,
            self.search_units,
            self.requests,
        ]
    }
    pub fn validate(&self) -> Result<(), BillingError> {
        if self.counts().iter().flatten().any(|n| *n > i64::MAX as u64) {
            return Err(BillingError::Overflow);
        }
        Ok(())
    }
    /// Refinement may add observations; it never erases or lowers one.
    pub fn preserves(&self, old: &Self) -> bool {
        self.validate().is_ok()
            && old
                .counts()
                .into_iter()
                .zip(self.counts())
                .all(|(old, new)| old.is_none_or(|old| new.is_some_and(|new| new >= old)))
    }
}
/// Bounded, copyable price variant label such as `1024x1024`, `768` or `1K`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MeterVariant {
    len: u8,
    bytes: [u8; MeterVariant::MAX],
}
impl MeterVariant {
    pub const MAX: usize = 32;
    pub fn new(s: &str) -> Option<Self> {
        if s.is_empty()
            || s.len() > Self::MAX
            || !s
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'-'))
        {
            return None;
        }
        let mut bytes = [0; Self::MAX];
        bytes[..s.len()].copy_from_slice(s.as_bytes());
        Some(Self {
            len: s.len() as u8,
            bytes,
        })
    }
    pub fn as_str(&self) -> &str {
        // Constructed only from validated ASCII.
        std::str::from_utf8(&self.bytes[..self.len as usize]).unwrap_or_default()
    }
}
impl std::fmt::Debug for MeterVariant {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self.as_str())
    }
}
impl Serialize for MeterVariant {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}
impl<'de> Deserialize<'de> for MeterVariant {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Self::new(&s).ok_or_else(|| serde::de::Error::custom("invalid meter variant"))
    }
}
/// Per-meter charges for non-token meters (pricing v3).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MeterCostComponents {
    #[serde(with = "money")]
    pub output_images_microusd: i64,
    #[serde(with = "money")]
    pub input_characters_microusd: i64,
    #[serde(with = "money")]
    pub input_audio_microusd: i64,
    #[serde(with = "money")]
    pub output_audio_microusd: i64,
    #[serde(with = "money")]
    pub search_units_microusd: i64,
    #[serde(with = "money")]
    pub requests_microusd: i64,
}
impl MeterCostComponents {
    pub fn values(&self) -> [i64; 6] {
        [
            self.output_images_microusd,
            self.input_characters_microusd,
            self.input_audio_microusd,
            self.output_audio_microusd,
            self.search_units_microusd,
            self.requests_microusd,
        ]
    }
}
/// Settled disjoint charges. V2 stores six token components; v3 adds six meter
/// components to the same JSON object (twelve keys).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CostBreakdown {
    Tokens(CostComponents),
    Metered(CostComponents, MeterCostComponents),
}
impl CostBreakdown {
    pub fn total(&self) -> Result<i64, BillingError> {
        match self {
            Self::Tokens(c) => c.total(),
            Self::Metered(c, m) => checked_total(c.values())?
                .checked_add(checked_total(m.values())?)
                .ok_or(BillingError::Overflow),
        }
    }
    pub fn to_value(&self) -> serde_json::Value {
        let mut v = serde_json::to_value(match self {
            Self::Tokens(c) | Self::Metered(c, _) => c,
        })
        .unwrap_or_default();
        if let (Self::Metered(_, m), Some(obj)) = (self, v.as_object_mut())
            && let Ok(serde_json::Value::Object(extra)) = serde_json::to_value(m)
        {
            obj.extend(extra);
        }
        v
    }
}
/// `ceil(units × microusd_per_batch / batch)` with checked i128 intermediates.
pub fn charge_batch(units: u64, rate: i64, batch: u64) -> Result<i64, BillingError> {
    if rate < 0 || batch == 0 {
        return Err(BillingError::InvalidRate);
    }
    if units > i64::MAX as u64 {
        return Err(BillingError::Overflow);
    }
    let batch = i128::from(batch);
    ((i128::from(units) * i128::from(rate) + batch - 1) / batch)
        .try_into()
        .map_err(|_| BillingError::Overflow)
}
fn checked_total(v: [i64; 6]) -> Result<i64, BillingError> {
    if v.iter().any(|n| *n < 0) {
        return Err(BillingError::InvalidRate);
    }
    v.into_iter()
        .map(i128::from)
        .sum::<i128>()
        .try_into()
        .map_err(|_| BillingError::Overflow)
}
pub fn charge(tokens: u64, rate: i64) -> Result<i64, BillingError> {
    if rate < 0 {
        return Err(BillingError::InvalidRate);
    }
    if tokens > i64::MAX as u64 {
        return Err(BillingError::Overflow);
    }
    ((i128::from(tokens) * i128::from(rate) + 999_999) / 1_000_000)
        .try_into()
        .map_err(|_| BillingError::Overflow)
}
fn valued(tokens: u64, rate: CacheRate) -> Result<Option<i64>, BillingError> {
    if tokens == 0 {
        return Ok(Some(0));
    }
    match rate {
        CacheRate::Priced {
            microusd_per_million,
        } => charge(tokens, microusd_per_million).map(Some),
        CacheRate::Unknown | CacheRate::NotApplicable => Ok(None),
    }
}
/// Complete v2 value. Missing allocations or positive usage at an unknown/NA rate stays unresolved.
pub fn value_v2(
    input_rate: i64,
    output_rate: i64,
    rates: &CachePricing,
    usage: &BillingUsage,
    output: Option<u64>,
) -> Result<Option<CostComponents>, BillingError> {
    rates.validate()?;
    charge(0, input_rate)?;
    charge(output.unwrap_or(0), output_rate)?;
    usage.validate()?;
    if !usage.is_complete() || output.is_none() {
        return Ok(None);
    }
    let parts = [
        usage.uncached_input_tokens.unwrap(),
        usage.cache_read_input_tokens.unwrap(),
        usage.cache_write_default_input_tokens.unwrap(),
        usage.cache_write_5m_input_tokens.unwrap(),
        usage.cache_write_1h_input_tokens.unwrap(),
        output.unwrap(),
    ];
    let prices = [
        CacheRate::Priced {
            microusd_per_million: input_rate,
        },
        rates.read,
        rates.write,
        rates.write_5m,
        rates.write_1h,
        CacheRate::Priced {
            microusd_per_million: output_rate,
        },
    ];
    let mut values = [0; 6];
    let mut unknown = false;
    for (i, (n, r)) in parts.into_iter().zip(prices).enumerate() {
        match valued(n, r)? {
            Some(v) => values[i] = v,
            None => unknown = true,
        }
    }
    checked_total(values)?;
    Ok((!unknown).then_some(CostComponents::from_values(values)))
}
/// Known disjoint charges only; this is a lower bound, never a reservation upper bound.
pub fn floor_v2(
    input_rate: i64,
    output_rate: i64,
    rates: &CachePricing,
    usage: Option<&BillingUsage>,
    output: Option<u64>,
) -> Result<i64, BillingError> {
    rates.validate()?;
    charge(0, input_rate)?;
    let mut amounts = [0; 6];
    amounts[5] = charge(output.unwrap_or(0), output_rate)?;
    if let Some(u) = usage {
        u.validate()?;
        amounts[0] = charge(u.uncached_input_tokens.unwrap_or(0), input_rate)?;
        for (i, (n, r)) in [
            u.cache_read_input_tokens,
            u.cache_write_default_input_tokens,
            u.cache_write_5m_input_tokens,
            u.cache_write_1h_input_tokens,
        ]
        .into_iter()
        .zip(rates.rates())
        .enumerate()
        {
            amounts[i + 1] = valued(n.unwrap_or(0), r)?.unwrap_or(0);
        }
    }
    checked_total(amounts)
}
/// Conservative independent category ceilings also cover per-component micro-USD rounding.
pub fn bound_v2(
    input_rate: i64,
    output_rate: i64,
    rates: &CachePricing,
    input_limit: u64,
    output_limit: u64,
) -> Result<Option<i64>, BillingError> {
    rates.validate()?;
    let mut amounts = [0; 6];
    amounts[0] = charge(input_limit, input_rate)?;
    amounts[5] = charge(output_limit, output_rate)?;
    let mut unknown = false;
    for (i, r) in rates.rates().into_iter().enumerate() {
        if matches!(r, CacheRate::NotApplicable) {
            continue;
        }
        match valued(input_limit, r)? {
            Some(v) => amounts[i + 1] = v,
            None => unknown = true,
        }
    }
    let total = checked_total(amounts)?;
    Ok((!unknown).then_some(total))
}

#[cfg(test)]
mod tests {
    #[test]
    fn json_requires_seven_explicit_nullable_counters() {
        let complete = serde_json::to_value(super::BillingUsage::default()).unwrap();
        assert!(serde_json::from_value::<super::BillingUsage>(complete.clone()).is_ok());
        assert!(serde_json::from_value::<super::BillingUsage>(serde_json::json!({})).is_err());
        for field in complete.as_object().unwrap().keys() {
            let mut missing = complete.clone();
            missing.as_object_mut().unwrap().remove(field);
            assert!(
                serde_json::from_value::<super::BillingUsage>(missing).is_err(),
                "{field}"
            );
        }
    }
    use super::*;
    fn priced(n: i64) -> CacheRate {
        CacheRate::Priced {
            microusd_per_million: n,
        }
    }
    fn rates() -> CachePricing {
        CachePricing {
            read: priced(100_000),
            write: priced(1_250_000),
            write_5m: priced(1_250_000),
            write_1h: priced(2_000_000),
        }
    }
    fn usage() -> BillingUsage {
        BillingUsage {
            total_input_tokens: Some(30),
            uncached_input_tokens: Some(10),
            cache_read_input_tokens: Some(5),
            cache_write_input_tokens: Some(15),
            cache_write_default_input_tokens: Some(3),
            cache_write_5m_input_tokens: Some(4),
            cache_write_1h_input_tokens: Some(8),
        }
    }
    #[test]
    fn partitions_and_exact_disjoint_charges() {
        let u = usage();
        assert!(u.is_complete());
        assert_eq!(u.input_lower_bound(), Ok(30));
        let c = value_v2(1_000_000, 3_000_000, &rates(), &u, Some(2))
            .unwrap()
            .unwrap();
        assert_eq!(c.values(), [10, 1, 4, 5, 16, 6]);
        assert_eq!(c.total(), Ok(42));
        assert_eq!(
            floor_v2(1_000_000, 3_000_000, &rates(), Some(&u), Some(2)),
            Ok(42)
        );
    }
    #[test]
    fn missing_and_unknown_are_never_zero() {
        let u = BillingUsage {
            cache_write_1h_input_tokens: None,
            ..usage()
        };
        assert!(!u.is_complete());
        assert_eq!(value_v2(1, 1, &rates(), &u, Some(1)), Ok(None));
        let r = CachePricing {
            read: CacheRate::Unknown,
            ..rates()
        };
        assert_eq!(bound_v2(1, 1, &r, 30, 2), Ok(None));
        assert_eq!(value_v2(1, 1, &r, &usage(), Some(2)), Ok(None));
        let r = CachePricing {
            read: CacheRate::NotApplicable,
            ..rates()
        };
        assert_eq!(value_v2(1, 1, &r, &usage(), Some(2)), Ok(None));
        assert_eq!(
            value_v2(
                0,
                0,
                &CachePricing::default(),
                &BillingUsage {
                    total_input_tokens: Some(0),
                    uncached_input_tokens: Some(0),
                    cache_read_input_tokens: Some(0),
                    cache_write_input_tokens: Some(0),
                    cache_write_default_input_tokens: Some(0),
                    cache_write_5m_input_tokens: Some(0),
                    cache_write_1h_input_tokens: Some(0)
                },
                Some(0)
            )
            .unwrap()
            .unwrap()
            .total(),
            Ok(0)
        );
    }
    #[test]
    fn invalid_counts_and_erasure_fail_closed() {
        assert_eq!(
            BillingUsage {
                cache_write_input_tokens: Some(14),
                ..usage()
            }
            .validate(),
            Err(BillingError::InvalidUsage)
        );
        assert_eq!(
            BillingUsage {
                total_input_tokens: Some(29),
                ..usage()
            }
            .validate(),
            Err(BillingError::InvalidUsage)
        );
        assert!(
            !BillingUsage {
                cache_read_input_tokens: None,
                ..usage()
            }
            .preserves(&usage())
        );
        assert!(
            !BillingUsage {
                cache_write_default_input_tokens: Some(2),
                cache_write_5m_input_tokens: Some(5),
                ..usage()
            }
            .preserves(&usage())
        );
        assert!(usage().preserves(&BillingUsage {
            cache_write_default_input_tokens: None,
            ..usage()
        }));
    }
    #[test]
    fn bound_covers_independent_rounding_and_partial_rate_overflow() {
        let r = CachePricing {
            read: priced(1),
            write: priced(1),
            write_5m: priced(1),
            write_1h: priced(1),
        };
        for n in 0..20 {
            for o in 0..5 {
                let u = BillingUsage {
                    total_input_tokens: Some(n * 5),
                    uncached_input_tokens: Some(n),
                    cache_read_input_tokens: Some(n),
                    cache_write_input_tokens: Some(n * 3),
                    cache_write_default_input_tokens: Some(n),
                    cache_write_5m_input_tokens: Some(n),
                    cache_write_1h_input_tokens: Some(n),
                };
                assert!(
                    value_v2(1, 1, &r, &u, Some(o))
                        .unwrap()
                        .unwrap()
                        .total()
                        .unwrap()
                        <= bound_v2(1, 1, &r, n * 5, o).unwrap().unwrap()
                );
            }
        }
        assert_eq!(
            bound_v2(
                0,
                0,
                &CachePricing {
                    read: CacheRate::Unknown,
                    write: priced(i64::MAX),
                    ..rates()
                },
                i64::MAX as u64,
                0
            ),
            Err(BillingError::Overflow)
        );
        assert_eq!(charge(u64::MAX, 0), Err(BillingError::Overflow));
        assert_eq!(charge(0, -1), Err(BillingError::InvalidRate));
    }
    #[test]
    fn json_is_exact_and_strict() {
        let v = serde_json::to_value(usage()).unwrap();
        assert_eq!(v["total_input_tokens"], "30");
        assert_eq!(serde_json::from_value::<BillingUsage>(v).unwrap(), usage());
        for v in [
            serde_json::json!({"status":"unknown","microusd_per_million":"0"}),
            serde_json::json!({"status":"not_applicable","microusd_per_million":"0"}),
            serde_json::json!({"status":"priced","microusd_per_million":1}),
        ] {
            assert!(serde_json::from_value::<CacheRate>(v).is_err());
        }
        assert!(
            serde_json::from_value::<CachePricing>(
                serde_json::json!({"read":{"status":"unknown"}})
            )
            .is_err()
        );
        assert!(
            serde_json::from_value::<BillingUsage>(
                serde_json::json!({"total_input_tokens":"9223372036854775808"})
            )
            .is_err()
        );
        assert!(
            serde_json::from_value::<BillingUsage>(serde_json::json!({"total_input_tokens":0}))
                .is_err()
        );
    }
}
