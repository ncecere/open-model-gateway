//! The billable meters: every unit a price line can charge, its tiers,
//! labels and which workloads can use it.
use super::*;

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
    pub(super) fn display_unit(self, batch: u64) -> String {
        if self.is_token() || self.is_audio_token() {
            format!("/M {}", self.noun())
        } else {
            self.unit_label(batch).unwrap_or("/unit").to_owned()
        }
    }
}
