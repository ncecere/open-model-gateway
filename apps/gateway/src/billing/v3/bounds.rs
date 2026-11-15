//! Admission ceilings: the conservative upper bound of a request's charge
//! under a price list, for tokens, realtime windows and video.
use super::*;

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
