//! Realtime response hold sizing from the session's actual context.
//!
//! Realtime input grows with the conversation: every response reads the whole
//! conversation so far. A fixed input ceiling per response is either far too
//! high for short sessions or too low for long ones. Each response's input
//! bound is therefore:
//!
//! - the conversation after the previous response, from its `response.done`
//!   usage (input plus output, per modality; cached tokens are part of each
//!   modality's total), or the context-window cap when that usage was unknown
//!   (never zero);
//! - plus everything the client sent since the previous `response.create`:
//!   text bytes as text tokens (a byte-level tokenizer never produces more
//!   tokens than bytes) and audio bytes as audio tokens at
//!   [`AUDIO_TOKENS_PER_SECOND`] for the slowest input format seen;
//! - plus the largest effective session configuration seen (instructions and
//!   tools: a per-response `instructions` override can hide them from the
//!   previous usage) and [`RESPONSE_OVERHEAD_TOKENS`] of framing.
//!
//! Each modality is capped at the context window (the price's input ceiling)
//! when the hold is valued. Output is bounded by the response's own
//! `max_output_tokens`, which the frontend always sets within the configured
//! window. A response whose usage exceeds its bound is recorded as unbounded,
//! exactly as before, so a wrong assumption is visible, not hidden.
use serde_json::Value;

use super::RealtimeUsage;

/// Conservative audio-token rate for client audio: 50 tokens per second.
/// OpenAI reports about 10 tokens/s for input audio and about 20 tokens/s for
/// output audio (a capped live check measured 45 output tokens for 2.25 s).
pub const AUDIO_TOKENS_PER_SECOND: u64 = 50;
/// The slowest allowed input format: G.711 (`audio/pcmu`, `audio/pcma`),
/// 8 kHz × 1 byte. Unknown formats are assumed to be this slow.
pub const SLOWEST_AUDIO_BYTES_PER_SECOND: u64 = 8_000;
/// `audio/pcm`: 24 kHz × 16-bit mono.
pub const PCM_AUDIO_BYTES_PER_SECOND: u64 = 48_000;
/// Per-response framing allowance (role markers, item separators, the
/// response's own prefix) in text tokens.
pub const RESPONSE_OVERHEAD_TOKENS: u64 = 256;

/// Token bound of one response window. `None` input means unknown context:
/// the hold uses the context-window cap for that modality.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResponseBound {
    pub text_input: Option<u64>,
    pub audio_input: Option<u64>,
    /// The response's `max_output_tokens` (text and audio output each).
    pub output: u32,
}
impl ResponseBound {
    /// The window reserved at admission, before anything was sent: an empty
    /// conversation plus framing. The first `response.create` resizes it.
    pub fn admission(output: u32) -> Self {
        Self {
            text_input: Some(RESPONSE_OVERHEAD_TOKENS),
            audio_input: Some(0),
            output,
        }
    }
    /// Unknown context: every input modality at the context-window cap.
    pub fn unknown(output: u32) -> Self {
        Self {
            text_input: None,
            audio_input: None,
            output,
        }
    }
    /// `(text input, audio input, output)` token ceilings, each input capped
    /// at the context window.
    pub fn ceilings(&self, context: u64) -> (u64, u64, u64) {
        let cap = |n: Option<u64>| n.map_or(context, |n| n.min(context));
        (
            cap(self.text_input),
            cap(self.audio_input),
            u64::from(self.output),
        )
    }
}

/// What one client event can add to the conversation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EventSize {
    /// Bytes of the event outside base64 audio payloads (JSON structure
    /// included, which also covers per-item framing).
    pub text_bytes: u64,
    /// Decoded-size upper bound of its base64 audio payloads.
    pub audio_bytes: u64,
    /// Byte rate of an input audio format the event selects.
    pub audio_bytes_per_second: Option<u64>,
}

/// Upper bound of the decoded size of `chars` base64 characters.
pub fn base64_decoded_bytes(chars: usize) -> u64 {
    (chars as u64).div_ceil(4).saturating_mul(3)
}

/// Audio tokens of `bytes` of input audio at `bytes_per_second`, rounded up.
pub fn audio_tokens(bytes: u64, bytes_per_second: u64) -> u64 {
    let rate = u128::from(bytes_per_second.max(1));
    let tokens = (u128::from(bytes) * u128::from(AUDIO_TOKENS_PER_SECOND)).div_ceil(rate);
    u64::try_from(tokens).unwrap_or(u64::MAX)
}

/// Byte rate of an OpenAI realtime input format (GA object or legacy string).
/// `None` when no format is given; unrecognized formats are the slowest.
pub fn input_format_rate(format: &Value) -> Option<u64> {
    let pcm = |rate: Option<u64>| {
        rate.filter(|r| *r > 0)
            .map_or(PCM_AUDIO_BYTES_PER_SECOND, |r| r.saturating_mul(2))
    };
    Some(match format {
        Value::Null => return None,
        Value::String(s) if s == "pcm16" => PCM_AUDIO_BYTES_PER_SECOND,
        Value::Object(o) => match o.get("type").and_then(Value::as_str) {
            Some("audio/pcm") => pcm(o.get("rate").and_then(Value::as_u64)),
            _ => SLOWEST_AUDIO_BYTES_PER_SECOND,
        },
        _ => SLOWEST_AUDIO_BYTES_PER_SECOND,
    })
}

/// Context-relevant parts of an effective session configuration.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SessionConfig {
    /// Bytes of `instructions` and `tools`: an upper bound on the text tokens
    /// they add to every response's input.
    pub text_bytes: u64,
    pub audio_bytes_per_second: Option<u64>,
}
/// Read an OpenAI realtime session object (`session.created`/`updated`).
pub fn session_config(session: &Value) -> SessionConfig {
    let instructions = session["instructions"].as_str().map_or(0, str::len);
    let tools = match &session["tools"] {
        Value::Null => 0,
        tools => tools.to_string().len(),
    };
    SessionConfig {
        text_bytes: (instructions as u64).saturating_add(tools as u64),
        audio_bytes_per_second: input_format_rate(&session["audio"]["input"]["format"]),
    }
}

/// Client input since a point in the session, as token upper bounds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Growth {
    pub text_tokens: u64,
    pub audio_tokens: u64,
}
impl Growth {
    fn add(&mut self, other: Growth) {
        self.text_tokens = self.text_tokens.saturating_add(other.text_tokens);
        self.audio_tokens = self.audio_tokens.saturating_add(other.audio_tokens);
    }
}

/// Conversation size after a response: its input plus its output, per
/// modality (the output joins the conversation).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Prior {
    pub text_tokens: u64,
    pub audio_tokens: u64,
}
impl From<RealtimeUsage> for Prior {
    fn from(u: RealtimeUsage) -> Self {
        Self {
            text_tokens: u.input_text_tokens.saturating_add(u.output_text_tokens),
            audio_tokens: u.input_audio_tokens.saturating_add(u.output_audio_tokens),
        }
    }
}

/// The input bound of the next response (see the module docs). Saturating:
/// the context-window cap applies when the hold is valued.
pub fn response_bound(
    prior: Option<Prior>,
    growth: Growth,
    config_text_bytes: u64,
    output: u32,
) -> ResponseBound {
    let Some(prior) = prior else {
        return ResponseBound::unknown(output);
    };
    ResponseBound {
        text_input: Some(
            prior
                .text_tokens
                .saturating_add(growth.text_tokens)
                .saturating_add(config_text_bytes)
                .saturating_add(RESPONSE_OVERHEAD_TOKENS),
        ),
        audio_input: Some(prior.audio_tokens.saturating_add(growth.audio_tokens)),
        output,
    }
}

/// Per-session tracker behind [`response_bound`].
#[derive(Clone, Debug)]
pub struct ContextTracker {
    /// `None` after a response with unknown usage.
    prior: Option<Prior>,
    /// Client input since the last forwarded `response.create`.
    growth: Growth,
    /// Input before a forwarded request that is neither created nor
    /// rejected; restored if upstream rejects the request.
    pending: Growth,
    config_text_bytes: u64,
    audio_bytes_per_second: u64,
}
impl ContextTracker {
    /// A new session: an empty conversation with the acknowledged
    /// configuration.
    pub fn new(config: SessionConfig) -> Self {
        Self {
            prior: Some(Prior::default()),
            growth: Growth::default(),
            pending: Growth::default(),
            config_text_bytes: config.text_bytes,
            audio_bytes_per_second: config
                .audio_bytes_per_second
                .unwrap_or(SLOWEST_AUDIO_BYTES_PER_SECOND),
        }
    }
    fn audio_rate(&mut self, rate: Option<u64>) {
        if let Some(rate) = rate {
            self.audio_bytes_per_second = self.audio_bytes_per_second.min(rate.max(1));
        }
    }
    /// An effective configuration reported by upstream: the largest
    /// configuration and the slowest audio format count.
    pub fn session_config(&mut self, config: SessionConfig) {
        self.config_text_bytes = self.config_text_bytes.max(config.text_bytes);
        self.audio_rate(config.audio_bytes_per_second);
    }
    /// A forwarded client event (including `response.create` itself).
    pub fn client_event(&mut self, size: EventSize) {
        // A format change applies to audio that follows it, possibly in the
        // same window: the slowest format seen bounds all of it.
        self.audio_rate(size.audio_bytes_per_second);
        self.growth.add(Growth {
            text_tokens: size.text_bytes,
            audio_tokens: audio_tokens(size.audio_bytes, self.audio_bytes_per_second),
        });
    }
    /// The bound of a `response.create` about to be forwarded. Input sent
    /// from here on belongs to the next response.
    pub fn request(&mut self, output: u32) -> ResponseBound {
        let bound = response_bound(self.prior, self.growth, self.config_text_bytes, output);
        let growth = std::mem::take(&mut self.growth);
        self.pending.add(growth);
        bound
    }
    /// Upstream rejected the request: its input is still unread.
    pub fn rejected(&mut self) {
        let pending = std::mem::take(&mut self.pending);
        self.growth.add(pending);
    }
    /// The request became a response: its input is in that response's usage.
    pub fn opened(&mut self) {
        self.pending = Growth::default();
    }
    /// `response.done`: known usage measures the conversation; unknown usage
    /// makes the next bound fall back to the context-window cap.
    pub fn done(&mut self, usage: Option<RealtimeUsage>) {
        self.prior = usage.map(Prior::from);
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const U: RealtimeUsage = RealtimeUsage {
        input_text_tokens: 1_000,
        cached_text_tokens: 600,
        input_audio_tokens: 300,
        cached_audio_tokens: 100,
        output_text_tokens: 50,
        output_audio_tokens: 200,
    };

    #[test]
    fn bound_adds_prior_context_growth_configuration_and_framing() {
        let growth = Growth {
            text_tokens: 120,
            audio_tokens: 40,
        };
        let b = response_bound(Some(Prior::from(U)), growth, 500, 64);
        // Cached tokens are part of each modality's total, never added twice.
        assert_eq!(b.text_input, Some(1_000 + 50 + 120 + 500 + 256));
        assert_eq!(b.audio_input, Some(300 + 200 + 40));
        assert_eq!(b.output, 64);
        // An empty conversation still holds framing, never zero text.
        let empty = response_bound(Some(Prior::default()), Growth::default(), 0, 10);
        assert_eq!(empty, ResponseBound::admission(10));
    }

    #[test]
    fn unknown_prior_usage_falls_back_to_the_context_cap() {
        let b = response_bound(None, Growth::default(), 0, 32);
        assert_eq!(b, ResponseBound::unknown(32));
        assert_eq!(b.ceilings(32_000), (32_000, 32_000, 32));
        // Known bounds are capped per modality at the context window.
        let big = ResponseBound {
            text_input: Some(40_000),
            audio_input: Some(10),
            output: 7,
        };
        assert_eq!(big.ceilings(32_000), (32_000, 10, 7));
        let huge = response_bound(
            Some(Prior {
                text_tokens: u64::MAX,
                audio_tokens: u64::MAX,
            }),
            Growth {
                text_tokens: u64::MAX,
                audio_tokens: 1,
            },
            u64::MAX,
            1,
        );
        assert_eq!(huge.ceilings(4_000), (4_000, 4_000, 1));
    }

    #[test]
    fn audio_tokens_use_a_conservative_rate_and_round_up() {
        // One second of G.711 or PCM16 is 50 tokens.
        assert_eq!(audio_tokens(8_000, SLOWEST_AUDIO_BYTES_PER_SECOND), 50);
        assert_eq!(audio_tokens(48_000, PCM_AUDIO_BYTES_PER_SECOND), 50);
        assert_eq!(audio_tokens(1, PCM_AUDIO_BYTES_PER_SECOND), 1);
        assert_eq!(audio_tokens(0, PCM_AUDIO_BYTES_PER_SECOND), 0);
        assert_eq!(audio_tokens(u64::MAX, 1), u64::MAX);
        assert_eq!(base64_decoded_bytes(0), 0);
        assert_eq!(base64_decoded_bytes(4), 3);
        assert_eq!(base64_decoded_bytes(5), 6);
        for (format, rate) in [
            (json!({"type":"audio/pcm","rate":24000}), Some(48_000)),
            (json!({"type":"audio/pcm"}), Some(48_000)),
            (json!({"type":"audio/pcm","rate":0}), Some(48_000)),
            (json!({"type":"audio/pcmu"}), Some(8_000)),
            (json!({"type":"audio/pcma"}), Some(8_000)),
            (json!({"type":"audio/opus"}), Some(8_000)),
            (json!("pcm16"), Some(48_000)),
            (json!("g711_ulaw"), Some(8_000)),
            (Value::Null, None),
        ] {
            assert_eq!(input_format_rate(&format), rate, "{format}");
        }
    }

    #[test]
    fn session_configuration_counts_instructions_and_tools() {
        let tools = json!([{"type":"function","name":"lookup","parameters":{}}]);
        let c = session_config(&json!({
            "instructions": "Be brief.",
            "tools": tools,
            "audio": {"input": {"format": {"type":"audio/pcmu"}}}
        }));
        assert_eq!(c.text_bytes, 9 + tools.to_string().len() as u64);
        assert_eq!(c.audio_bytes_per_second, Some(8_000));
        assert_eq!(session_config(&json!({})), SessionConfig::default());
    }

    #[test]
    fn tracker_follows_requests_rejections_and_usage() {
        let mut t = ContextTracker::new(SessionConfig {
            text_bytes: 100,
            audio_bytes_per_second: Some(PCM_AUDIO_BYTES_PER_SECOND),
        });
        t.client_event(EventSize {
            text_bytes: 200,
            audio_bytes: 96_000,
            audio_bytes_per_second: None,
        });
        let first = t.request(64);
        assert_eq!(first.text_input, Some(200 + 100 + 256));
        assert_eq!(first.audio_input, Some(100));
        // A rejected request's input is not lost.
        t.rejected();
        t.client_event(EventSize {
            text_bytes: 10,
            ..EventSize::default()
        });
        let retry = t.request(64);
        assert_eq!(retry.text_input, Some(210 + 100 + 256));
        t.opened();
        // Input sent while the response runs belongs to the next one.
        t.client_event(EventSize {
            text_bytes: 30,
            ..EventSize::default()
        });
        t.done(Some(U));
        let second = t.request(64);
        assert_eq!(second.text_input, Some(1_050 + 30 + 100 + 256));
        assert_eq!(second.audio_input, Some(500));
        t.opened();
        // A format switch slows every later audio estimate; a larger
        // configuration counts from then on.
        t.session_config(SessionConfig {
            text_bytes: 1_000,
            audio_bytes_per_second: Some(SLOWEST_AUDIO_BYTES_PER_SECOND),
        });
        t.client_event(EventSize {
            text_bytes: 0,
            audio_bytes: 8_000,
            audio_bytes_per_second: Some(PCM_AUDIO_BYTES_PER_SECOND),
        });
        t.done(Some(U));
        let third = t.request(64);
        assert_eq!(third.text_input, Some(1_050 + 1_000 + 256));
        assert_eq!(third.audio_input, Some(500 + 50));
        t.opened();
        // Unknown usage: the next bound is the context cap, never zero.
        t.done(None);
        assert_eq!(t.request(64), ResponseBound::unknown(64));
    }
}
