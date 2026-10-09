//! Pricing v3: immutable OpenRouter-style price lines with explicit units.
//!
//! A missing meter line is **unknown**, not free; `"0"` is explicitly free and
//! `not_applicable` asserts the meter cannot apply. Charges are exact integer
//! micro-USD: `ceil(count × microusd_per_batch / batch)` per meter.
use super::{
    AudioTokenCostComponents, BillingError, BillingUsage, CachePricing, CacheRate, CostComponents,
    MeterCostComponents, MeterUsage, MeterVariant, charge_batch,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Meter {
    #[serde(rename = "input_tokens")]
    InputTokens,
    #[serde(rename = "output_tokens")]
    OutputTokens,
    #[serde(rename = "cache_read_tokens")]
    CacheReadTokens,
    #[serde(rename = "cache_write_tokens")]
    CacheWriteTokens,
    #[serde(rename = "cache_write_5m_tokens")]
    CacheWrite5mTokens,
    #[serde(rename = "cache_write_1h_tokens")]
    CacheWrite1hTokens,
    #[serde(rename = "output_images")]
    OutputImages,
    #[serde(rename = "input_characters")]
    InputCharacters,
    #[serde(rename = "input_audio_seconds_ms")]
    InputAudioSecondsMs,
    #[serde(rename = "output_audio_seconds_ms")]
    OutputAudioSecondsMs,
    #[serde(rename = "search_units")]
    SearchUnits,
    #[serde(rename = "requests")]
    Requests,
    /// Realtime-only audio-token meters (see [`Meter::AUDIO_TOKENS`]).
    #[serde(rename = "input_audio_tokens")]
    InputAudioTokens,
    #[serde(rename = "cache_read_audio_tokens")]
    CacheReadAudioTokens,
    #[serde(rename = "output_audio_tokens")]
    OutputAudioTokens,
    /// Generated video duration of async video jobs (migration 0016); see
    /// [`Meter::VIDEO`].
    #[serde(rename = "output_video_seconds_ms")]
    OutputVideoSecondsMs,
}
pub const MAX_LINES: usize = 64;
const AUDIO_BATCHES: &[u64] = &[1_000, 60_000, 3_600_000];
impl Meter {
    pub const ALL: [Meter; 12] = [
        Meter::InputTokens,
        Meter::OutputTokens,
        Meter::CacheReadTokens,
        Meter::CacheWriteTokens,
        Meter::CacheWrite5mTokens,
        Meter::CacheWrite1hTokens,
        Meter::OutputImages,
        Meter::InputCharacters,
        Meter::InputAudioSecondsMs,
        Meter::OutputAudioSecondsMs,
        Meter::SearchUnits,
        Meter::Requests,
    ];
    /// Realtime audio tokens (uncached input, cached input, output), priced
    /// per million like text tokens. They apply only to the realtime workload;
    /// every other workload ignores them, so they are not in [`Meter::ALL`].
    /// No `min_prompt_tokens` tiers and no `max_units`: their ceilings are the
    /// price's token ceilings.
    pub const AUDIO_TOKENS: [Meter; 3] = [
        Meter::InputAudioTokens,
        Meter::CacheReadAudioTokens,
        Meter::OutputAudioTokens,
    ];
    pub fn is_audio_token(self) -> bool {
        Self::AUDIO_TOKENS.contains(&self)
    }
    /// Video seconds apply only to the video workload (variant = resolution);
    /// every other workload cannot produce video, so the meter is not in
    /// [`Meter::ALL`] and prices without a video line keep working.
    pub const VIDEO: [Meter; 1] = [Meter::OutputVideoSecondsMs];
    /// Meters that may carry a `variant` (closed tier set per price).
    pub fn has_variants(self) -> bool {
        matches!(self, Self::OutputImages | Self::OutputVideoSecondsMs)
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InputTokens => "input_tokens",
            Self::OutputTokens => "output_tokens",
            Self::CacheReadTokens => "cache_read_tokens",
            Self::CacheWriteTokens => "cache_write_tokens",
            Self::CacheWrite5mTokens => "cache_write_5m_tokens",
            Self::CacheWrite1hTokens => "cache_write_1h_tokens",
            Self::OutputImages => "output_images",
            Self::InputCharacters => "input_characters",
            Self::InputAudioSecondsMs => "input_audio_seconds_ms",
            Self::OutputAudioSecondsMs => "output_audio_seconds_ms",
            Self::SearchUnits => "search_units",
            Self::Requests => "requests",
            Self::InputAudioTokens => "input_audio_tokens",
            Self::CacheReadAudioTokens => "cache_read_audio_tokens",
            Self::OutputAudioTokens => "output_audio_tokens",
            Self::OutputVideoSecondsMs => "output_video_seconds_ms",
        }
    }
    pub fn is_token(self) -> bool {
        matches!(
            self,
            Self::InputTokens
                | Self::OutputTokens
                | Self::CacheReadTokens
                | Self::CacheWriteTokens
                | Self::CacheWrite5mTokens
                | Self::CacheWrite1hTokens
        )
    }
    /// Allowed batch sizes; the canonical unit label follows from the batch.
    pub fn batches(self) -> &'static [u64] {
        match self {
            m if m.is_token() || m.is_audio_token() => &[1_000_000],
            Self::InputCharacters => &[1_000_000],
            Self::InputAudioSecondsMs | Self::OutputAudioSecondsMs | Self::OutputVideoSecondsMs => {
                AUDIO_BATCHES
            }
            _ => &[1],
        }
    }
    pub fn unit_label(self, batch: u64) -> Option<&'static str> {
        if !self.batches().contains(&batch) {
            return None;
        }
        Some(match self {
            m if m.is_token() || m.is_audio_token() => "/M tokens",
            Self::OutputImages => "/image",
            Self::InputCharacters => "/M characters",
            Self::InputAudioSecondsMs | Self::OutputAudioSecondsMs | Self::OutputVideoSecondsMs => {
                match batch {
                    1_000 => "/second",
                    60_000 => "/minute",
                    _ => "/hour",
                }
            }
            Self::SearchUnits => "/search",
            _ => "/request",
        })
    }
    pub fn noun(self) -> &'static str {
        match self {
            Self::InputTokens => "input tokens",
            Self::OutputTokens => "output tokens",
            Self::CacheReadTokens => "cache read tokens",
            Self::CacheWriteTokens => "cache write tokens",
            Self::CacheWrite5mTokens => "5-minute cache write tokens",
            Self::CacheWrite1hTokens => "1-hour cache write tokens",
            Self::OutputImages => "output images",
            Self::InputCharacters => "input characters",
            Self::InputAudioSecondsMs => "input audio",
            Self::OutputAudioSecondsMs => "output audio",
            Self::SearchUnits => "search units",
            Self::Requests => "requests",
            Self::InputAudioTokens => "input audio tokens",
            Self::CacheReadAudioTokens => "cached input audio tokens",
            Self::OutputAudioTokens => "output audio tokens",
            Self::OutputVideoSecondsMs => "output video",
        }
    }
    fn display_unit(self, batch: u64) -> String {
        if self.is_token() || self.is_audio_token() {
            format!("/M {}", self.noun())
        } else {
            self.unit_label(batch).unwrap_or("/unit").to_owned()
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rate {
    pub microusd_per_batch: i64,
    pub batch: u64,
}
/// One validated line. `rate: None` is an explicit not-applicable assertion.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PriceLine {
    pub meter: Meter,
    pub rate: Option<Rate>,
    pub sku_label: Option<String>,
    pub variant: Option<MeterVariant>,
    pub min_prompt_tokens: Option<u64>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLine {
    meter: Meter,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    microusd_per_batch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    batch: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    unit_label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sku_label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    variant: Option<MeterVariant>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    min_prompt_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    not_applicable: bool,
}
fn label_valid(s: &str) -> bool {
    !s.trim().is_empty() && s.chars().count() <= 80 && !s.chars().any(char::is_control)
}
fn money(s: &str) -> Option<i64> {
    if s.is_empty() || s.len() > 19 || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}
impl TryFrom<RawLine> for PriceLine {
    type Error = BillingError;
    fn try_from(r: RawLine) -> Result<Self, BillingError> {
        let bad = Err(BillingError::InvalidRate);
        if r.not_applicable {
            // Exactly `{meter, not_applicable:true}`: NA is the meter's only line.
            if r.microusd_per_batch.is_some()
                || r.batch.is_some()
                || r.unit_label.is_some()
                || r.sku_label.is_some()
                || r.variant.is_some()
                || r.min_prompt_tokens.is_some()
            {
                return bad;
            }
            return Ok(Self {
                meter: r.meter,
                rate: None,
                sku_label: None,
                variant: None,
                min_prompt_tokens: None,
            });
        }
        let (Some(amount), Some(batch), Some(unit), Some(sku)) =
            (r.microusd_per_batch, r.batch, r.unit_label, r.sku_label)
        else {
            return bad;
        };
        let Some(amount) = money(&amount) else {
            return bad;
        };
        if r.meter.unit_label(batch) != Some(unit.as_str())
            || !label_valid(&sku)
            || (r.variant.is_some() && !r.meter.has_variants())
            || (r.min_prompt_tokens.is_some() && r.meter.is_audio_token())
            || r.min_prompt_tokens
                .is_some_and(|n| !(1..=i32::MAX as u64).contains(&n))
        {
            return bad;
        }
        Ok(Self {
            meter: r.meter,
            rate: Some(Rate {
                microusd_per_batch: amount,
                batch,
            }),
            sku_label: Some(sku),
            variant: r.variant,
            min_prompt_tokens: r.min_prompt_tokens,
        })
    }
}
impl From<&PriceLine> for RawLine {
    fn from(l: &PriceLine) -> Self {
        Self {
            meter: l.meter,
            microusd_per_batch: l.rate.map(|r| r.microusd_per_batch.to_string()),
            batch: l.rate.map(|r| r.batch),
            unit_label: l
                .rate
                .and_then(|r| l.meter.unit_label(r.batch))
                .map(str::to_owned),
            sku_label: l.sku_label.clone(),
            variant: l.variant,
            min_prompt_tokens: l.min_prompt_tokens,
            not_applicable: l.rate.is_none(),
        }
    }
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PriceLines(pub Vec<PriceLine>);
impl PriceLines {
    pub fn validate(&self) -> Result<(), BillingError> {
        if self.0.is_empty() || self.0.len() > MAX_LINES {
            return Err(BillingError::InvalidRate);
        }
        let mut keys = BTreeSet::new();
        for l in &self.0 {
            if !keys.insert((l.meter, l.variant, l.min_prompt_tokens))
                || l.rate
                    .is_some_and(|r| r.microusd_per_batch < 0 || r.batch == 0)
            {
                return Err(BillingError::InvalidRate);
            }
        }
        // Audio-token lines extend a price; they never stand alone.
        if self.0.iter().all(|l| l.meter.is_audio_token()) {
            return Err(BillingError::InvalidRate);
        }
        for m in Meter::ALL
            .into_iter()
            .chain(Meter::AUDIO_TOKENS)
            .chain(Meter::VIDEO)
        {
            let n = self.0.iter().filter(|l| l.meter == m).count();
            if n > 1 && self.0.iter().any(|l| l.meter == m && l.rate.is_none()) {
                return Err(BillingError::InvalidRate);
            }
        }
        Ok(())
    }
    fn of(&self, meter: Meter) -> Vec<&PriceLine> {
        self.0.iter().filter(|l| l.meter == meter).collect()
    }
    /// The price asserts this meter cannot apply.
    pub fn not_applicable(&self, meter: Meter) -> bool {
        matches!(group(self, meter, None), Group::NotApplicable)
    }
    /// Every input-family token meter (uncached input, cache read and cache
    /// writes) is explicitly not applicable, so the input token ceiling may be
    /// zero. A missing line is unknown, not inapplicable.
    pub fn input_tokens_inapplicable(&self) -> bool {
        Meter::ALL
            .into_iter()
            .filter(|m| m.is_token() && *m != Meter::OutputTokens)
            .all(|m| self.not_applicable(m))
    }
    /// Every line is a zero rate or not applicable (an explicitly free price).
    pub fn all_free_or_not_applicable(&self) -> bool {
        Meter::ALL.into_iter().all(|m| match group(self, m, None) {
            Group::NotApplicable => true,
            Group::Missing => false,
            Group::Lines(_) => self
                .of(m)
                .iter()
                .all(|l| l.rate.is_some_and(|r| r.microusd_per_batch == 0)),
        }) && self
            .of(Meter::OutputVideoSecondsMs)
            .iter()
            .all(|l| l.rate.is_none_or(|r| r.microusd_per_batch == 0))
    }
}
impl Serialize for PriceLines {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        self.0
            .iter()
            .map(RawLine::from)
            .collect::<Vec<_>>()
            .serialize(s)
    }
}
impl<'de> Deserialize<'de> for PriceLines {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let lines = Vec::<RawLine>::deserialize(d)?
            .into_iter()
            .map(PriceLine::try_from)
            .collect::<Result<Vec<_>, _>>()
            .map(Self)
            .map_err(|_| serde::de::Error::custom("invalid price line"))?;
        lines
            .validate()
            .map_err(|_| serde::de::Error::custom("invalid price lines"))?;
        Ok(lines)
    }
}
/// Trusted per-request ceilings for non-token meters, decimal strings in JSON.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MaxUnits(pub BTreeMap<Meter, u64>);
impl Serialize for MaxUnits {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        self.0
            .iter()
            .map(|(k, v)| (*k, v.to_string()))
            .collect::<BTreeMap<_, _>>()
            .serialize(s)
    }
}
impl<'de> Deserialize<'de> for MaxUnits {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        BTreeMap::<Meter, String>::deserialize(d)?
            .into_iter()
            .map(|(k, v)| match money(&v) {
                Some(n) if !k.is_token() && !k.is_audio_token() => Ok((k, n as u64)),
                _ => Err(serde::de::Error::custom("invalid max_units")),
            })
            .collect::<Result<_, _>>()
            .map(Self)
    }
}
enum Group<'a> {
    NotApplicable,
    Missing,
    Lines(Vec<&'a PriceLine>),
}
fn group(lines: &PriceLines, meter: Meter, variant: Option<MeterVariant>) -> Group<'_> {
    let all = lines.of(meter);
    if all.is_empty() {
        return Group::Missing;
    }
    if all.iter().any(|l| l.rate.is_none()) {
        return Group::NotApplicable;
    }
    if !meter.has_variants() {
        return Group::Lines(all);
    }
    let specific: Vec<_> = all
        .iter()
        .copied()
        .filter(|l| variant.is_some() && l.variant == variant)
        .collect();
    if !specific.is_empty() {
        return Group::Lines(specific);
    }
    // Variant-less lines are the default for unlisted or unreported variants.
    Group::Lines(all.into_iter().filter(|l| l.variant.is_none()).collect())
}
enum Tier<'a> {
    Line(&'a PriceLine),
    Undetermined,
    Unpriced,
}
/// Highest strictly-exceeded `min_prompt_tokens` threshold wins per meter.
fn tier<'a>(g: &[&'a PriceLine], prompt: Option<u64>) -> Tier<'a> {
    if g.is_empty() {
        return Tier::Unpriced;
    }
    if prompt.is_none() && g.iter().any(|l| l.min_prompt_tokens.is_some()) {
        return Tier::Undetermined;
    }
    let prompt = prompt.unwrap_or(0);
    g.iter()
        .copied()
        .filter(|l| l.min_prompt_tokens.is_none_or(|m| prompt > m))
        .max_by_key(|l| l.min_prompt_tokens)
        .map_or(Tier::Unpriced, Tier::Line)
}
fn free(g: &[&PriceLine]) -> bool {
    !g.is_empty()
        && g.iter()
            .all(|l| l.rate.is_some_and(|r| r.microusd_per_batch == 0))
}
/// Realtime audio-token observations (`None` = unknown, never zero).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AudioTokens {
    /// Uncached input audio tokens.
    pub input: Option<u64>,
    /// Cached input audio tokens (a disjoint partition, not a subset).
    pub cache_read: Option<u64>,
    pub output: Option<u64>,
}
#[derive(Clone, Copy, Default)]
pub struct Observed<'a> {
    pub billing: Option<&'a BillingUsage>,
    pub output_tokens: Option<u64>,
    pub meters: Option<&'a MeterUsage>,
    pub variant: Option<MeterVariant>,
    /// `Some` only for the realtime workload: the audio-token meters apply
    /// and are valued like other meters. `None` (every other workload) means
    /// they cannot apply, whatever the price lists.
    pub audio: Option<AudioTokens>,
    /// `true` only for the video workload: `output_video_seconds_ms` applies
    /// (read from `meters`). Every other workload cannot produce video.
    pub video: bool,
}
impl Observed<'_> {
    fn count(&self, meter: Meter) -> Option<u64> {
        let b = self.billing;
        let m = self.meters;
        match meter {
            Meter::InputTokens => b.and_then(|b| b.uncached_input_tokens),
            Meter::OutputTokens => self.output_tokens,
            Meter::CacheReadTokens => b.and_then(|b| b.cache_read_input_tokens),
            Meter::CacheWriteTokens => b.and_then(|b| b.cache_write_default_input_tokens),
            Meter::CacheWrite5mTokens => b.and_then(|b| b.cache_write_5m_input_tokens),
            Meter::CacheWrite1hTokens => b.and_then(|b| b.cache_write_1h_input_tokens),
            Meter::OutputImages => m.and_then(|m| m.output_images),
            Meter::InputCharacters => m.and_then(|m| m.input_characters),
            Meter::InputAudioSecondsMs => m.and_then(|m| m.input_audio_seconds_ms),
            Meter::OutputAudioSecondsMs => m.and_then(|m| m.output_audio_seconds_ms),
            Meter::SearchUnits => m.and_then(|m| m.search_units),
            Meter::Requests => m.and_then(|m| m.requests),
            Meter::InputAudioTokens => self.audio.and_then(|a| a.input),
            Meter::CacheReadAudioTokens => self.audio.and_then(|a| a.cache_read),
            Meter::OutputAudioTokens => self.audio.and_then(|a| a.output),
            Meter::OutputVideoSecondsMs => m.and_then(|m| m.output_video_seconds_ms),
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Valuation {
    /// Complete settlement, or `None` when any possibly-used meter is unresolved.
    pub components: Option<(CostComponents, MeterCostComponents)>,
    /// Known charges only: a lower bound, never a reservation upper bound.
    pub floor: i64,
    /// The admission hold can no longer be proven to cover this observation.
    pub violated: bool,
    /// Audio-token charges (realtime only; zero otherwise). Part of `floor`
    /// and of a complete settlement.
    pub audio: AudioTokenCostComponents,
}
pub fn value(
    lines: &PriceLines,
    max: &MaxUnits,
    obs: &Observed,
) -> Result<Valuation, BillingError> {
    lines.validate()?;
    if let Some(b) = obs.billing {
        b.validate()?;
    }
    if let Some(m) = obs.meters {
        m.validate()?;
    }
    if obs.output_tokens.is_some_and(|n| n > i64::MAX as u64)
        || obs.audio.is_some_and(|a| {
            [a.input, a.cache_read, a.output]
                .into_iter()
                .flatten()
                .any(|n| n > i64::MAX as u64)
        })
    {
        return Err(BillingError::Overflow);
    }
    let prompt = obs.billing.and_then(|b| b.total_input_tokens);
    // Indices: Meter::ALL 0..12, audio tokens 12..15, video 15.
    let mut amounts = [0i64; 16];
    let mut complete = true;
    let mut violated = false;
    let audio_meters: &[Meter] = if obs.audio.is_some() {
        &Meter::AUDIO_TOKENS
    } else {
        &[]
    };
    let video_meters: &[Meter] = if obs.video { &Meter::VIDEO } else { &[] };
    let residual_input = residual_uncached_input(lines, obs);
    for (i, meter) in Meter::ALL
        .into_iter()
        .chain(audio_meters.iter().copied())
        .enumerate()
        .chain(video_meters.iter().map(|m| (15, *m)))
    {
        let count = match meter {
            Meter::InputTokens => obs.count(meter).or(residual_input),
            _ => obs.count(meter),
        };
        match group(lines, meter, obs.variant) {
            Group::NotApplicable => {
                if count.is_some_and(|n| n > 0) {
                    complete = false;
                    violated = true;
                }
            }
            Group::Missing => {
                if count != Some(0) {
                    complete = false;
                    violated |= count.is_some_and(|n| n > 0);
                }
            }
            Group::Lines(g) => match count {
                Some(0) => {}
                _ if free(&g) => {}
                None => complete = false,
                Some(n) => match tier(&g, prompt) {
                    Tier::Line(l) => {
                        let r = l.rate.ok_or(BillingError::InvalidRate)?;
                        amounts[i] = charge_batch(n, r.microusd_per_batch, r.batch)?;
                    }
                    Tier::Undetermined => complete = false,
                    Tier::Unpriced => {
                        complete = false;
                        violated = true;
                    }
                },
            },
        }
        if !meter.is_token()
            && !meter.is_audio_token()
            && let (Some(n), Some(limit)) = (count, max.0.get(&meter))
            && n > *limit
        {
            violated = true;
        }
    }
    // A not-applicable uncached input contradicts unexplained inclusive input.
    if let Some(b) = obs.billing
        && matches!(group(lines, Meter::InputTokens, None), Group::NotApplicable)
        && b.uncached_input_tokens.is_none()
        && let Some(total) = b.total_input_tokens
    {
        let writes = b
            .write_parts()
            .into_iter()
            .flatten()
            .sum::<u64>()
            .max(b.cache_write_input_tokens.unwrap_or(0));
        if total
            > b.cache_read_input_tokens
                .unwrap_or(0)
                .saturating_add(writes)
        {
            complete = false;
            violated = true;
        }
    }
    let floor: i64 = amounts
        .iter()
        .map(|n| i128::from(*n))
        .sum::<i128>()
        .try_into()
        .map_err(|_| BillingError::Overflow)?;
    let components = complete.then_some((
        CostComponents {
            uncached_input_microusd: amounts[0],
            cache_read_microusd: amounts[2],
            cache_write_default_microusd: amounts[3],
            cache_write_5m_microusd: amounts[4],
            cache_write_1h_microusd: amounts[5],
            output_microusd: amounts[1],
        },
        MeterCostComponents {
            output_images_microusd: amounts[6],
            input_characters_microusd: amounts[7],
            input_audio_microusd: amounts[8],
            output_audio_microusd: amounts[9],
            search_units_microusd: amounts[10],
            requests_microusd: amounts[11],
            output_video_microusd: amounts[15],
        },
    ));
    Ok(Valuation {
        components,
        floor,
        violated,
        audio: AudioTokenCostComponents {
            input_audio_tokens_microusd: amounts[12],
            cache_read_audio_tokens_microusd: amounts[13],
            output_audio_tokens_microusd: amounts[14],
        },
    })
}
/// Uncached input that the price itself proves: a provider that reports only
/// the inclusive input total (no cache split, as local SGLang/vLLM servers do)
/// leaves uncached input unknown. When every cache category with an unknown
/// count is `not_applicable`, those categories are absent, so uncached input is
/// the total minus the known cache parts. An aggregate write that its known
/// allocations do not explain would force a residual into an NA category, so
/// nothing is derived then (it stays unknown, never zero).
fn residual_uncached_input(lines: &PriceLines, obs: &Observed) -> Option<u64> {
    let b = obs.billing?;
    let total = b.total_input_tokens?;
    let read = match b.cache_read_input_tokens {
        Some(n) => n,
        None if lines.not_applicable(Meter::CacheReadTokens) => 0,
        None => return None,
    };
    let mut writes = 0u64;
    for (meter, count) in [
        (Meter::CacheWriteTokens, b.cache_write_default_input_tokens),
        (Meter::CacheWrite5mTokens, b.cache_write_5m_input_tokens),
        (Meter::CacheWrite1hTokens, b.cache_write_1h_input_tokens),
    ] {
        match count {
            Some(n) => writes = writes.checked_add(n)?,
            None if lines.not_applicable(meter) => {}
            None => return None,
        }
    }
    if b.cache_write_input_tokens.is_some_and(|w| w > writes) {
        return None;
    }
    total.checked_sub(read.checked_add(writes)?)
}
/// Conservative admission ceiling: every possible input-family token meter is
/// charged on the full input ceiling at its highest applicable tier rate, output
/// on the requested output maximum, and each unit meter on `max_units` at its
/// highest (variant) rate. Any possible meter without a price or ceiling is
/// unbounded (`None`). Zero-rate meters need no ceiling.
pub fn bound(
    lines: &PriceLines,
    max: &MaxUnits,
    input_limit: u64,
    output_limit: u64,
) -> Result<Option<i64>, BillingError> {
    bound_meters(
        lines,
        max,
        TokenCeilings::uniform(input_limit, output_limit),
        &Meter::ALL,
    )
}
/// Token ceilings of one realtime response window, per modality. Each input
/// modality is a total that includes its cached part: both the uncached and
/// the cached meter of a modality are charged on the full total, so the bound
/// holds for any cache split.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RealtimeCeilings {
    pub text_input: u64,
    pub audio_input: u64,
    /// Text and audio output (each charged on the full output ceiling).
    pub output: u64,
}
/// [`bound`] for one realtime response window: text-input meters (uncached,
/// cached and the impossible cache writes) on the text ceiling, audio-input
/// meters on the audio ceiling and both output meters on the output ceiling.
/// Prompt tiers apply at the combined input.
pub fn bound_realtime(
    lines: &PriceLines,
    max: &MaxUnits,
    c: RealtimeCeilings,
) -> Result<Option<i64>, BillingError> {
    let mut meters = Meter::ALL.to_vec();
    meters.extend(Meter::AUDIO_TOKENS);
    bound_meters(
        lines,
        max,
        TokenCeilings {
            input: c.text_input,
            audio_input: c.audio_input,
            output: c.output,
            prompt: c.text_input.saturating_add(c.audio_input),
        },
        &meters,
    )
}
/// [`bound`] for one async video job: additionally `output_video_seconds_ms`
/// on its `max_units` ceiling at the highest (resolution variant) rate.
pub fn bound_video(
    lines: &PriceLines,
    max: &MaxUnits,
    input_limit: u64,
    output_limit: u64,
) -> Result<Option<i64>, BillingError> {
    let mut meters = Meter::ALL.to_vec();
    meters.extend(Meter::VIDEO);
    bound_meters(
        lines,
        max,
        TokenCeilings::uniform(input_limit, output_limit),
        &meters,
    )
}
/// Token-meter ceilings of [`bound_meters`]; `prompt` selects prompt tiers.
#[derive(Clone, Copy)]
struct TokenCeilings {
    input: u64,
    audio_input: u64,
    output: u64,
    prompt: u64,
}
impl TokenCeilings {
    fn uniform(input: u64, output: u64) -> Self {
        Self {
            input,
            audio_input: input,
            output,
            prompt: input,
        }
    }
}
fn bound_meters(
    lines: &PriceLines,
    max: &MaxUnits,
    c: TokenCeilings,
    meters: &[Meter],
) -> Result<Option<i64>, BillingError> {
    lines.validate()?;
    let input_limit = c.prompt;
    let mut total: i128 = 0;
    let mut unknown = false;
    for meter in meters.iter().copied() {
        let ceiling = if matches!(meter, Meter::OutputTokens | Meter::OutputAudioTokens) {
            Some(c.output)
        } else if meter.is_audio_token() {
            Some(c.audio_input)
        } else if meter.is_token() {
            Some(c.input)
        } else {
            max.0.get(&meter).copied()
        };
        let all = lines.of(meter);
        if all.iter().any(|l| l.rate.is_none()) || ceiling == Some(0) {
            continue;
        }
        if all.is_empty() {
            unknown = true;
            continue;
        }
        // Every priced variant group must cover small prompts with a base line.
        let variants: BTreeSet<_> = all.iter().map(|l| l.variant).collect();
        if variants.into_iter().any(|v| {
            !all.iter()
                .any(|l| l.variant == v && l.min_prompt_tokens.is_none())
        }) {
            unknown = true;
            continue;
        }
        let applicable: Vec<_> = all
            .into_iter()
            .filter(|l| l.min_prompt_tokens.is_none_or(|m| input_limit > m))
            .collect();
        if free(&applicable) {
            continue;
        }
        let Some(ceiling) = ceiling else {
            unknown = true;
            continue;
        };
        let mut highest = 0;
        for l in applicable {
            let r = l.rate.ok_or(BillingError::InvalidRate)?;
            highest = highest.max(charge_batch(ceiling, r.microusd_per_batch, r.batch)?);
        }
        total += i128::from(highest);
    }
    let total: i64 = total.try_into().map_err(|_| BillingError::Overflow)?;
    Ok((!unknown).then_some(total))
}
/// Cache categories as v2 rate states, for the shared residual-possibility check.
pub fn cache_rates(lines: &PriceLines) -> CachePricing {
    let rate = |m| match group(lines, m, None) {
        Group::NotApplicable => CacheRate::NotApplicable,
        Group::Lines(g) if g.iter().any(|l| l.min_prompt_tokens.is_none()) => CacheRate::Priced {
            microusd_per_million: 0,
        },
        _ => CacheRate::Unknown,
    };
    CachePricing {
        read: rate(Meter::CacheReadTokens),
        write: rate(Meter::CacheWriteTokens),
        write_5m: rate(Meter::CacheWrite5mTokens),
        write_1h: rate(Meter::CacheWrite1hTokens),
    }
}
/// Exact USD from integer micro-USD: whole dollars without decimals, otherwise
/// at least two decimals with no unnecessary trailing zeros.
pub fn format_usd(microusd: i64) -> String {
    let (sign, n) = if microusd < 0 {
        ("-", microusd.unsigned_abs())
    } else {
        ("", microusd as u64)
    };
    let (whole, frac) = (n / 1_000_000, n % 1_000_000);
    if frac == 0 {
        return format!("{sign}${whole}");
    }
    let mut digits = format!("{frac:06}");
    while digits.len() > 2 && digits.ends_with('0') {
        digits.pop();
    }
    format!("{sign}${whole}.{digits}")
}
fn grouped(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}
/// Human display such as `$0.10/M input tokens` or `$0.0205/image (768)`.
pub fn display_line(l: &PriceLine) -> String {
    let noun = || {
        let noun = l.meter.noun();
        let mut c = noun.chars();
        let first = c.next().map(|f| f.to_ascii_uppercase()).unwrap_or(' ');
        format!("{first}{}", c.as_str())
    };
    let Some(r) = l.rate else {
        return format!("{}: not applicable", noun());
    };
    // An explicit zero rate reads "Free" everywhere (never "$0/unit").
    let mut s = if r.microusd_per_batch == 0 {
        format!("{}: Free", noun())
    } else {
        format!(
            "{}{}",
            format_usd(r.microusd_per_batch),
            l.meter.display_unit(r.batch)
        )
    };
    let mut qualifiers = Vec::new();
    if let Some(v) = l.variant {
        qualifiers.push(v.as_str().to_owned());
    }
    if let Some(m) = l.min_prompt_tokens {
        qualifiers.push(format!("prompt > {} tokens", grouped(m)));
    }
    if !qualifiers.is_empty() {
        s.push_str(&format!(" ({})", qualifiers.join(", ")));
    }
    s
}
pub fn display_summary(lines: &PriceLines) -> String {
    lines
        .0
        .iter()
        .map(display_line)
        .collect::<Vec<_>>()
        .join(" · ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn lines(v: serde_json::Value) -> PriceLines {
        serde_json::from_value(v).unwrap()
    }
    fn tokens() -> serde_json::Value {
        json!([
            {"meter":"input_tokens","microusd_per_batch":"100000","batch":1000000,"unit_label":"/M tokens","sku_label":"Input"},
            {"meter":"input_tokens","microusd_per_batch":"500000","batch":1000000,"unit_label":"/M tokens","sku_label":"Input >100K","min_prompt_tokens":100000},
            {"meter":"output_tokens","microusd_per_batch":"500000","batch":1000000,"unit_label":"/M tokens","sku_label":"Output"},
            {"meter":"cache_read_tokens","microusd_per_batch":"10000","batch":1000000,"unit_label":"/M tokens","sku_label":"Cache read"},
            {"meter":"cache_write_tokens","not_applicable":true},
            {"meter":"cache_write_5m_tokens","not_applicable":true},
            {"meter":"cache_write_1h_tokens","not_applicable":true},
            {"meter":"output_images","not_applicable":true},
            {"meter":"input_characters","not_applicable":true},
            {"meter":"input_audio_seconds_ms","not_applicable":true},
            {"meter":"output_audio_seconds_ms","not_applicable":true},
            {"meter":"search_units","not_applicable":true},
            {"meter":"requests","microusd_per_batch":"0","batch":1,"unit_label":"/request","sku_label":"Request"}
        ])
    }
    fn billing(total: u64, uncached: u64, read: u64) -> BillingUsage {
        BillingUsage {
            total_input_tokens: Some(total),
            uncached_input_tokens: Some(uncached),
            cache_read_input_tokens: Some(read),
            cache_write_input_tokens: Some(0),
            cache_write_default_input_tokens: Some(0),
            cache_write_5m_input_tokens: Some(0),
            cache_write_1h_input_tokens: Some(0),
        }
    }
    #[test]
    fn strict_line_validation() {
        assert_eq!(serde_json::to_value(lines(tokens())).unwrap(), tokens());
        let base = json!({"meter":"output_images","microusd_per_batch":"20500","batch":1,"unit_label":"/image","sku_label":"Image"});
        for bad in [
            json!([]),
            json!([{"meter":"bogus","not_applicable":true}]),
            json!([{"meter":"output_images","not_applicable":false}]),
            json!([{"meter":"output_images","not_applicable":true,"sku_label":"x"}]),
            json!([{"meter":"output_images","microusd_per_batch":"1.5","batch":1,"unit_label":"/image","sku_label":"I"}]),
            json!([{"meter":"output_images","microusd_per_batch":20500,"batch":1,"unit_label":"/image","sku_label":"I"}]),
            json!([{"meter":"output_images","microusd_per_batch":"-1","batch":1,"unit_label":"/image","sku_label":"I"}]),
            json!([{"meter":"output_images","microusd_per_batch":"1","batch":2,"unit_label":"/image","sku_label":"I"}]),
            json!([{"meter":"input_tokens","microusd_per_batch":"1","batch":1000,"unit_label":"/M tokens","sku_label":"I"}]),
            json!([{"meter":"input_tokens","microusd_per_batch":"1","batch":1000000,"unit_label":"/tokens","sku_label":"I"}]),
            json!([{"meter":"input_tokens","microusd_per_batch":"1","batch":1000000,"unit_label":"/M tokens","sku_label":"I","variant":"1K"}]),
            json!([{"meter":"input_tokens","microusd_per_batch":"1","batch":1000000,"unit_label":"/M tokens","sku_label":"I","min_prompt_tokens":0}]),
            json!([{"meter":"input_tokens","microusd_per_batch":"1","batch":1000000,"unit_label":"/M tokens","sku_label":""}]),
            json!([{"meter":"input_tokens","microusd_per_batch":"1","batch":1000000,"unit_label":"/M tokens","sku_label":"I","extra":1}]),
            json!([base, base]),
            json!([base, {"meter":"output_images","not_applicable":true}]),
        ] {
            assert!(
                serde_json::from_value::<PriceLines>(bad.clone()).is_err(),
                "{bad}"
            );
        }
        let mut variant = base.clone();
        variant["variant"] = json!("1K");
        assert!(serde_json::from_value::<PriceLines>(json!([base, variant])).is_ok());
        for ok in [1_000u64, 60_000, 3_600_000] {
            let label = Meter::InputAudioSecondsMs.unit_label(ok).unwrap();
            assert!(serde_json::from_value::<PriceLines>(json!([{"meter":"input_audio_seconds_ms","microusd_per_batch":"200","batch":ok,"unit_label":label,"sku_label":"Audio"}])).is_ok());
        }
        assert!(serde_json::from_value::<MaxUnits>(json!({"input_tokens":"4"})).is_err());
        assert!(serde_json::from_value::<MaxUnits>(json!({"output_images":4})).is_err());
        assert_eq!(
            serde_json::to_value(
                serde_json::from_value::<MaxUnits>(json!({"output_images":"4"})).unwrap()
            )
            .unwrap(),
            json!({"output_images":"4"})
        );
    }
    #[test]
    fn exact_charges_tiers_free_and_unknown() {
        let l = lines(tokens());
        let max = MaxUnits::default();
        let b = billing(30, 20, 10);
        let v = value(
            &l,
            &max,
            &Observed {
                billing: Some(&b),
                output_tokens: Some(3),
                ..Default::default()
            },
        )
        .unwrap();
        // ceil(20×0.1)=2, ceil(10×0.01)=1, ceil(3×0.5)=2
        let (c, m) = v.components.unwrap();
        assert_eq!(
            (
                c.uncached_input_microusd,
                c.cache_read_microusd,
                c.output_microusd
            ),
            (2, 1, 2)
        );
        assert_eq!(m, MeterCostComponents::default());
        assert_eq!(v.floor, 5);
        assert!(!v.violated);
        // Tier applies only when prompt strictly exceeds the threshold.
        let at = billing(100_000, 100_000, 0);
        let over = billing(100_001, 100_001, 0);
        let charge = |b: &BillingUsage| {
            value(
                &l,
                &max,
                &Observed {
                    billing: Some(b),
                    output_tokens: Some(0),
                    ..Default::default()
                },
            )
            .unwrap()
            .components
            .unwrap()
            .0
            .uncached_input_microusd
        };
        assert_eq!(charge(&at), 10_000);
        assert_eq!(charge(&over), 50_001);
        // Unknown prompt with tiers is unresolved; floor keeps known parts.
        let partial = BillingUsage {
            total_input_tokens: None,
            ..b
        };
        let v = value(
            &l,
            &max,
            &Observed {
                billing: Some(&partial),
                output_tokens: Some(3),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!((v.components, v.floor, v.violated), (None, 3, false));
        // Missing line for a used meter: unknown, floor kept.
        let mut no_output = l.clone();
        no_output.0.retain(|x| x.meter != Meter::OutputTokens);
        let v = value(
            &no_output,
            &max,
            &Observed {
                billing: Some(&b),
                output_tokens: Some(3),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!((v.components, v.floor), (None, 3));
        // Positive NA usage contradicts the price.
        let meters = MeterUsage {
            search_units: Some(1),
            ..Default::default()
        };
        let v = value(
            &l,
            &max,
            &Observed {
                billing: Some(&b),
                output_tokens: Some(3),
                meters: Some(&meters),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(v.components.is_none() && v.violated);
    }
    #[test]
    fn total_only_input_settles_when_every_unknown_cache_category_is_na() {
        // A local server reported 59 inclusive input tokens and no cache split
        // (SGLang's `prompt_tokens_details: null`, as normalized live).
        let local = BillingUsage {
            total_input_tokens: Some(59),
            uncached_input_tokens: None,
            cache_read_input_tokens: None,
            cache_write_input_tokens: None,
            cache_write_default_input_tokens: None,
            cache_write_5m_input_tokens: Some(0),
            cache_write_1h_input_tokens: Some(0),
        };
        let observe = |l: &PriceLines, b: &BillingUsage| {
            value(
                l,
                &MaxUnits::default(),
                &Observed {
                    billing: Some(b),
                    output_tokens: Some(21),
                    ..Default::default()
                },
            )
            .unwrap()
        };
        let mut all_na = tokens();
        all_na[3] = json!({"meter":"cache_read_tokens","not_applicable":true});
        let all_na = lines(all_na);
        // ceil(59×0.1)=6, ceil(21×0.5)=11
        let v = observe(&all_na, &local);
        let (c, _) = v.components.expect("settles");
        assert_eq!((c.uncached_input_microusd, c.output_microusd), (6, 11));
        assert_eq!((v.floor, v.violated), (17, false));
        // Known cache parts are subtracted from the total.
        let read = BillingUsage {
            cache_read_input_tokens: Some(9),
            ..local
        };
        let (c, _) = observe(&lines(tokens()), &read).components.unwrap();
        assert_eq!((c.uncached_input_microusd, c.cache_read_microusd), (5, 1));
        // A priced cache read with an unknown count stays unresolved.
        let v = observe(&lines(tokens()), &local);
        assert_eq!((v.components, v.floor), (None, 11));
        // An unexplained aggregate write is a residual forced into an NA
        // category: nothing is derived, never valued as zero.
        let write = BillingUsage {
            cache_write_input_tokens: Some(4),
            ..local
        };
        assert!(observe(&all_na, &write).components.is_none());
        // Cache parts above the total are invalid usage, never a residual.
        let over = BillingUsage {
            cache_read_input_tokens: Some(60),
            ..local
        };
        let observed = Observed {
            billing: Some(&over),
            output_tokens: Some(21),
            ..Default::default()
        };
        assert!(value(&all_na, &MaxUnits::default(), &observed).is_err());
    }
    #[test]
    fn image_variants_and_unit_bounds() {
        let l = lines(json!([
            {"meter":"input_tokens","not_applicable":true},
            {"meter":"output_tokens","not_applicable":true},
            {"meter":"cache_read_tokens","not_applicable":true},
            {"meter":"cache_write_tokens","not_applicable":true},
            {"meter":"cache_write_5m_tokens","not_applicable":true},
            {"meter":"cache_write_1h_tokens","not_applicable":true},
            {"meter":"output_images","microusd_per_batch":"20500","batch":1,"unit_label":"/image","sku_label":"Image","variant":"768"},
            {"meter":"output_images","microusd_per_batch":"303500","batch":1,"unit_label":"/image","sku_label":"Image","variant":"4K"},
            {"meter":"input_characters","not_applicable":true},
            {"meter":"input_audio_seconds_ms","not_applicable":true},
            {"meter":"output_audio_seconds_ms","not_applicable":true},
            {"meter":"search_units","not_applicable":true},
            {"meter":"requests","not_applicable":true}
        ]));
        let mut max = MaxUnits::default();
        assert_eq!(bound(&l, &max, 100, 0), Ok(None));
        max.0.insert(Meter::OutputImages, 4);
        assert_eq!(bound(&l, &max, 100, 0), Ok(Some(4 * 303_500)));
        let meters = MeterUsage {
            output_images: Some(2),
            ..Default::default()
        };
        let obs = |v: &str| Observed {
            meters: Some(&meters),
            variant: MeterVariant::new(v),
            ..Default::default()
        };
        let v = value(&l, &max, &obs("768")).unwrap();
        assert_eq!(v.components.unwrap().1.output_images_microusd, 41_000);
        let v = value(&l, &max, &obs("2K")).unwrap();
        assert!(v.components.is_none() && v.violated);
        let many = MeterUsage {
            output_images: Some(5),
            ..Default::default()
        };
        let v = value(
            &l,
            &max,
            &Observed {
                meters: Some(&many),
                variant: MeterVariant::new("768"),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(v.components.is_some() && v.violated);
    }
    #[test]
    fn token_bound_uses_highest_applicable_tier_and_rounding() {
        let l = lines(tokens());
        let max = MaxUnits::default();
        // Tier >100K cannot apply under a 100K ceiling.
        assert_eq!(bound(&l, &max, 100_000, 10), Ok(Some(10_000 + 1_000 + 5)));
        assert_eq!(bound(&l, &max, 100_001, 10), Ok(Some(50_001 + 1_001 + 5)));
        // A tier without a base line leaves small prompts unpriced.
        let mut tier_only = l.clone();
        tier_only
            .0
            .retain(|x| !(x.meter == Meter::InputTokens && x.min_prompt_tokens.is_none()));
        assert_eq!(bound(&tier_only, &max, 200_000, 10), Ok(None));
        // Unit meter without a ceiling is unbounded unless free.
        let mut chars = l.clone();
        chars.0.retain(|x| x.meter != Meter::InputCharacters);
        chars.0.push(PriceLine {
            meter: Meter::InputCharacters,
            rate: Some(Rate {
                microusd_per_batch: 15_000_000,
                batch: 1_000_000,
            }),
            sku_label: Some("Characters".into()),
            variant: None,
            min_prompt_tokens: None,
        });
        assert_eq!(bound(&chars, &max, 100, 10), Ok(None));
        let mut max = MaxUnits::default();
        max.0.insert(Meter::InputCharacters, 4096);
        // ceil(4096 × 15) = 61,440 µUSD
        assert_eq!(
            bound(&chars, &max, 100, 10).unwrap().unwrap()
                - bound(&l, &MaxUnits::default(), 100, 10).unwrap().unwrap(),
            61_440
        );
        // Every observation within ceilings is covered by the bound.
        for input in [0u64, 1, 50, 100] {
            for out in [0u64, 1, 10] {
                let b = billing(input, input / 2, input - input / 2);
                let v = value(
                    &l,
                    &MaxUnits::default(),
                    &Observed {
                        billing: Some(&b),
                        output_tokens: Some(out),
                        ..Default::default()
                    },
                )
                .unwrap();
                assert!(v.floor <= bound(&l, &MaxUnits::default(), 100, 10).unwrap().unwrap());
            }
        }
        let mut huge = l.clone();
        huge.0[0].rate = Some(Rate {
            microusd_per_batch: i64::MAX,
            batch: 1_000_000,
        });
        assert_eq!(
            bound(&huge, &MaxUnits::default(), i64::MAX as u64, 0),
            Err(BillingError::Overflow)
        );
    }
    #[test]
    fn display_is_exact_without_floats() {
        assert_eq!(format_usd(100_000), "$0.10");
        assert_eq!(format_usd(20_500), "$0.0205");
        assert_eq!(format_usd(15_000_000), "$15");
        assert_eq!(format_usd(200_000), "$0.20");
        assert_eq!(format_usd(1), "$0.000001");
        assert_eq!(format_usd(i64::MAX), "$9223372036854.775807");
        let l = lines(json!([
            {"meter":"input_tokens","microusd_per_batch":"100000","batch":1000000,"unit_label":"/M tokens","sku_label":"Input"},
            {"meter":"input_tokens","microusd_per_batch":"500000","batch":1000000,"unit_label":"/M tokens","sku_label":"Input","min_prompt_tokens":272000},
            {"meter":"output_images","microusd_per_batch":"20500","batch":1,"unit_label":"/image","sku_label":"Image","variant":"768"},
            {"meter":"input_characters","microusd_per_batch":"15000000","batch":1000000,"unit_label":"/M characters","sku_label":"Characters"},
            {"meter":"input_audio_seconds_ms","microusd_per_batch":"200000","batch":60000,"unit_label":"/minute","sku_label":"Audio"},
            {"meter":"search_units","not_applicable":true},
            {"meter":"requests","microusd_per_batch":"0","batch":1,"unit_label":"/request","sku_label":"Request"},
            {"meter":"output_images","microusd_per_batch":"0","batch":1,"unit_label":"/image","sku_label":"Image","variant":"1K"}
        ]));
        assert_eq!(
            l.0.iter().map(display_line).collect::<Vec<_>>(),
            [
                "$0.10/M input tokens",
                "$0.50/M input tokens (prompt > 272,000 tokens)",
                "$0.0205/image (768)",
                "$15/M characters",
                "$0.20/minute",
                "Search units: not applicable",
                "Requests: Free",
                "Output images: Free (1K)"
            ]
        );
    }

    #[test]
    fn realtime_bound_uses_per_modality_ceilings() {
        let line = |m: &str, n: &str| json!({"meter":m,"microusd_per_batch":n,"batch":1000000,"unit_label":"/M tokens","sku_label":"L"});
        let l = lines(json!([
            line("input_tokens", "4000000"),
            line("cache_read_tokens", "400000"),
            line("output_tokens", "16000000"),
            line("input_audio_tokens", "32000000"),
            line("cache_read_audio_tokens", "400000"),
            line("output_audio_tokens", "64000000"),
            {"meter":"cache_write_tokens","not_applicable":true},
            {"meter":"cache_write_5m_tokens","not_applicable":true},
            {"meter":"cache_write_1h_tokens","not_applicable":true},
        ]));
        let mut max = MaxUnits::default();
        for meter in Meter::ALL.into_iter().filter(|m| !m.is_token()) {
            max.0.insert(meter, 0);
        }
        let c = |text_input, audio_input, output| RealtimeCeilings {
            text_input,
            audio_input,
            output,
        };
        // Text (uncached and cached) on the text ceiling, audio on the audio
        // ceiling, both outputs on the output ceiling; each meter rounded up.
        assert_eq!(
            bound_realtime(&l, &max, c(1_001, 7, 3)),
            Ok(Some(4_004 + 401 + 224 + 3 + 48 + 192))
        );
        // A zero audio ceiling holds nothing for audio input.
        assert_eq!(
            bound_realtime(&l, &max, c(10, 0, 1)),
            Ok(Some(40 + 4 + 16 + 64))
        );
        // The uniform bound is the same as equal per-modality ceilings.
        assert_eq!(
            bound_realtime(&l, &max, c(1_000, 1_000, 100)),
            Ok(Some(44_800))
        );
        // Monotonic: a larger context never holds less.
        assert!(
            bound_realtime(&l, &max, c(5_000, 200, 100)).unwrap()
                > bound_realtime(&l, &max, c(4_000, 200, 100)).unwrap()
        );
    }
}
