//! Property tests of the Anthropic Messages stream parser, `State::push` (#593).
//!
//! The contract these tests state, read from the code and from `execute`'s
//! loop:
//!
//! * `push` never panics. Every error is `InvalidUpstream`, except that an
//!   upstream `error` event on a Messages-protocol stream is classified by
//!   `anthropic_error` (`ContextLength`, `Overloaded`, `Busy`); upstream text is
//!   never reflected.
//! * Termination. `Finish` is emitted by the one `message_delta` that carries a
//!   `stop_reason`; `Usage` and `Done` by the `message_stop` that follows it.
//!   After `Done` every later frame is refused with `InvalidUpstream` (`State`
//!   has a `done` flag). Frames running out before `message_stop` is
//!   `framing::frames`' final `InvalidUpstream`, never a success, and the parser
//!   fabricates nothing for a truncated stream.
//! * Usage is merged from what was reported: the start event's `output_tokens`
//!   is preliminary and never used (absent output stays unknown, not zero), a
//!   cumulative counter may not go down, and input and cache counters
//!   round-trip exactly.
//! * Content. Text deltas and tool-call argument fragments are delivered in
//!   order; a tool block's fragments concatenate to a JSON object (an empty one
//!   is delivered as `{}`); in Messages mode every `content_block_*` frame is
//!   preserved verbatim in order. Limits refuse rather than truncate: 256
//!   blocks, 128 tools, and the response limit on tool arguments.
use super::*;
use crate::{
    billing::BillingUsage,
    proptest_support_tests::{
        Ev, Outcome, apply_all, arb_json, check_terminals, config, drive, leaves, mutation,
        split_text,
    },
};
use proptest::prelude::*;
use std::collections::BTreeSet;

// ---- spec ------------------------------------------------------------------

#[derive(Clone, Debug)]
enum Piece {
    Text {
        initial: String,
        pieces: Vec<String>,
    },
    Tool {
        name: String,
        args: Value,
        cuts: Vec<usize>,
        empty: bool,
    },
    Thinking {
        pieces: Vec<String>,
        signature: bool,
    },
    Redacted(String),
}

#[derive(Clone, Debug)]
enum CacheWrite {
    None,
    Total(u64),
    Alloc(u64, u64),
}

#[derive(Clone, Debug)]
struct Start {
    input: Option<u64>,
    output: Option<u64>,
    read: Option<u64>,
    write: CacheWrite,
}

#[derive(Clone, Debug)]
struct Delta {
    output: Option<u64>,
    input_extra: Option<u64>,
    repeat_cache: bool,
    thinking: Option<u64>,
}

#[derive(Clone, Debug)]
struct Spec {
    messages: bool,
    blocks: Vec<Piece>,
    stop: usize,
    start: Option<Start>,
    delta: Option<Delta>,
    pings: Vec<usize>,
}

const STOPS: [(&str, FinishReason); 6] = [
    ("end_turn", FinishReason::Stop),
    ("stop_sequence", FinishReason::Stop),
    ("max_tokens", FinishReason::Length),
    ("tool_use", FinishReason::ToolCalls),
    ("refusal", FinishReason::Refusal),
    (
        "model_context_window_exceeded",
        FinishReason::ContextWindowExceeded,
    ),
];

fn text() -> impl Strategy<Value = String> {
    prop_oneof![
        3 => "[ -~\n]{0,12}",
        1 => prop::collection::vec(any::<char>(), 0..6).prop_map(|v| v.into_iter().collect::<String>()),
    ]
}

fn piece() -> impl Strategy<Value = Piece> {
    prop_oneof![
        4 => (text(), prop::collection::vec(text(), 0..4))
            .prop_map(|(initial, pieces)| Piece::Text { initial, pieces }),
        3 => ("[A-Za-z0-9_-]{1,12}", arb_json(), prop::collection::vec(any::<usize>(), 0..4), 0..5_u8)
            .prop_map(|(name, args, cuts, empty)| Piece::Tool {
                name,
                args: if args.is_object() { args } else { serde_json::json!({ "value": args }) },
                cuts,
                empty: empty == 0,
            }),
        1 => (prop::collection::vec(text(), 0..3), any::<bool>())
            .prop_map(|(pieces, signature)| Piece::Thinking { pieces, signature }),
        1 => text().prop_map(Piece::Redacted),
    ]
}

fn count() -> impl Strategy<Value = u64> {
    prop_oneof![3 => 0..5000_u64, 1 => 0..=(i64::MAX as u64 / 8)]
}

fn start() -> impl Strategy<Value = Start> {
    (
        prop::option::of(count()),
        prop::option::of(0..100_u64),
        prop::option::of(count()),
        prop_oneof![
            Just(CacheWrite::None),
            count().prop_map(CacheWrite::Total),
            (count(), count()).prop_map(|(a, b)| CacheWrite::Alloc(a, b)),
        ],
    )
        .prop_map(|(input, output, read, write)| Start {
            input,
            output,
            read,
            write,
        })
}

fn delta() -> impl Strategy<Value = Delta> {
    (
        prop::option::of(count()),
        prop::option::of(0..1000_u64),
        any::<bool>(),
        prop::option::of(any::<u64>()),
    )
        .prop_map(|(output, input_extra, repeat_cache, thinking)| Delta {
            output,
            input_extra,
            repeat_cache,
            thinking: thinking.map(|t| t % (output.unwrap_or(1000) + 1)),
        })
}

fn spec() -> impl Strategy<Value = Spec> {
    (
        any::<bool>(),
        prop::collection::vec(piece(), 0..6),
        0..STOPS.len(),
        prop::option::of(start()),
        prop::option::of(delta()),
        prop::collection::vec(any::<usize>(), 0..3),
    )
        .prop_map(|(messages, blocks, stop, start, delta, pings)| Spec {
            messages,
            blocks,
            stop,
            start,
            delta,
            pings,
        })
}

// ---- building the wire stream and what it must produce -----------------------

fn start_usage(s: &Start) -> Value {
    let mut u = serde_json::Map::new();
    if let Some(n) = s.input {
        u.insert("input_tokens".into(), n.into());
    }
    if let Some(n) = s.output {
        u.insert("output_tokens".into(), n.into());
    }
    if let Some(n) = s.read {
        u.insert("cache_read_input_tokens".into(), n.into());
    }
    match s.write {
        CacheWrite::None => {}
        CacheWrite::Total(w) => {
            u.insert("cache_creation_input_tokens".into(), w.into());
        }
        CacheWrite::Alloc(a, b) => {
            u.insert("cache_creation_input_tokens".into(), (a + b).into());
            u.insert(
                "cache_creation".into(),
                json!({"ephemeral_5m_input_tokens": a, "ephemeral_1h_input_tokens": b}),
            );
        }
    }
    Value::Object(u)
}

fn delta_usage(d: &Delta, s: &Option<Start>) -> Value {
    let mut u = serde_json::Map::new();
    if let Some(n) = d.output {
        u.insert("output_tokens".into(), n.into());
    }
    if let (Some(s), Some(extra)) = (s, d.input_extra)
        && let Some(i) = s.input
    {
        u.insert("input_tokens".into(), (i + extra).into());
    }
    if let (Some(s), true) = (s, d.repeat_cache) {
        let mut repeated = start_usage(s);
        repeated.as_object_mut().unwrap().remove("input_tokens");
        repeated.as_object_mut().unwrap().remove("output_tokens");
        u.extend(repeated.as_object().unwrap().clone());
    }
    if let Some(t) = d.thinking {
        u.insert(
            "output_tokens_details".into(),
            json!({"thinking_tokens": t}),
        );
    }
    Value::Object(u)
}

/// The merged usage the stream must report, from the reported numbers alone.
fn expected_usage(spec: &Spec) -> Usage {
    if spec.start.is_none() && spec.delta.is_none() {
        return Usage::default();
    }
    let s = spec.start.as_ref();
    let d = spec.delta.as_ref();
    let input = match (s.and_then(|s| s.input), d.and_then(|d| d.input_extra)) {
        (Some(i), Some(extra)) => Some(i + extra),
        (i, _) => i,
    };
    let read = s.and_then(|s| s.read);
    let (write, default, five, hour) = match s.map(|s| s.write.clone()) {
        Some(CacheWrite::Total(w)) => (Some(w), None, None, None),
        Some(CacheWrite::Alloc(a, b)) => (Some(a + b), Some(0), Some(a), Some(b)),
        _ => (None, None, None, None),
    };
    let total = match (input, read, write) {
        (Some(i), Some(r), Some(w)) => Some(i + r + w),
        _ => None,
    };
    let output = d.and_then(|d| d.output);
    Usage {
        input_tokens: input,
        output_tokens: output,
        billing: Some(BillingUsage {
            total_input_tokens: total,
            uncached_input_tokens: input,
            cache_read_input_tokens: read,
            cache_write_input_tokens: write,
            cache_write_default_input_tokens: default,
            cache_write_5m_input_tokens: five,
            cache_write_1h_input_tokens: hour,
        }),
        reasoning_tokens: d
            .and_then(|d| d.thinking)
            .filter(|t| output.is_none_or(|o| *t <= o)),
        ..Usage::default()
    }
}

struct Built {
    events: Vec<Value>,
    expected: Vec<Ev>,
}

/// Push `event` and, in Messages mode, record that it is preserved verbatim.
fn native(spec: &Spec, events: &mut Vec<Value>, expected: &mut Vec<Ev>, event: Value) {
    if spec.messages {
        expected.push(Ev::Block(event.clone()));
    }
    events.push(event);
}

fn build(spec: &Spec) -> Built {
    let mut events = vec![];
    let mut expected = vec![];
    let mut start = json!({"id": "msg_1", "type": "message", "role": "assistant", "model": "claude-x",
        "content": [], "stop_reason": null, "stop_sequence": null});
    if let Some(s) = &spec.start {
        start["usage"] = start_usage(s);
    }
    events.push(json!({"type": "message_start", "message": start}));
    let mut tools = 0_u32;
    let mut index = 0_usize;
    for block in &spec.blocks {
        match block {
            Piece::Text { initial, pieces } => {
                native(
                    spec,
                    &mut events,
                    &mut expected,
                    json!({"type": "content_block_start", "index": index, "content_block": {"type": "text", "text": initial}}),
                );
                if !initial.is_empty() {
                    expected.push(Ev::Text(initial.clone()));
                }
                for p in pieces {
                    native(
                        spec,
                        &mut events,
                        &mut expected,
                        json!({"type": "content_block_delta", "index": index, "delta": {"type": "text_delta", "text": p}}),
                    );
                    expected.push(Ev::Text(p.clone()));
                }
            }
            Piece::Tool {
                name,
                args,
                cuts,
                empty,
            } => {
                let id = format!("toolu_{index}");
                native(
                    spec,
                    &mut events,
                    &mut expected,
                    json!({"type": "content_block_start", "index": index,
                    "content_block": {"type": "tool_use", "id": id, "name": name, "input": {}}}),
                );
                expected.push(Ev::ToolStart {
                    index: tools,
                    id,
                    name: name.clone(),
                });
                let pieces = if *empty {
                    vec![]
                } else {
                    split_text(&args.to_string(), cuts)
                };
                for p in &pieces {
                    native(
                        spec,
                        &mut events,
                        &mut expected,
                        json!({"type": "content_block_delta", "index": index,
                        "delta": {"type": "input_json_delta", "partial_json": p}}),
                    );
                    expected.push(Ev::ToolArgs {
                        index: tools,
                        args: p.clone(),
                    });
                }
                native(
                    spec,
                    &mut events,
                    &mut expected,
                    json!({"type": "content_block_stop", "index": index}),
                );
                if pieces.concat().is_empty() {
                    expected.push(Ev::ToolArgs {
                        index: tools,
                        args: "{}".into(),
                    });
                }
                tools += 1;
                index += 1;
                continue;
            }
            Piece::Thinking { pieces, signature } if spec.messages => {
                native(
                    spec,
                    &mut events,
                    &mut expected,
                    json!({"type": "content_block_start", "index": index,
                    "content_block": {"type": "thinking", "thinking": "", "signature": ""}}),
                );
                for p in pieces {
                    native(
                        spec,
                        &mut events,
                        &mut expected,
                        json!({"type": "content_block_delta", "index": index,
                        "delta": {"type": "thinking_delta", "thinking": p}}),
                    );
                }
                if *signature {
                    native(
                        spec,
                        &mut events,
                        &mut expected,
                        json!({"type": "content_block_delta", "index": index,
                        "delta": {"type": "signature_delta", "signature": "sig"}}),
                    );
                }
            }
            Piece::Redacted(data) if spec.messages => {
                native(
                    spec,
                    &mut events,
                    &mut expected,
                    json!({"type": "content_block_start", "index": index,
                    "content_block": {"type": "redacted_thinking", "data": data}}),
                );
            }
            // Thinking blocks exist only on Messages streams.
            Piece::Thinking { .. } | Piece::Redacted(_) => continue,
        }
        native(
            spec,
            &mut events,
            &mut expected,
            json!({"type": "content_block_stop", "index": index}),
        );
        index += 1;
    }
    let (reason, finish) = STOPS[spec.stop];
    let mut message_delta =
        json!({"type": "message_delta", "delta": {"stop_reason": reason, "stop_sequence": null}});
    if let Some(d) = &spec.delta {
        message_delta["usage"] = delta_usage(d, &spec.start);
    }
    events.push(message_delta);
    expected.push(Ev::Finish(finish));
    events.push(json!({"type": "message_stop"}));
    expected.extend([Ev::Usage(expected_usage(spec)), Ev::Done]);
    // Pings may arrive anywhere; they emit nothing.
    for p in &spec.pings {
        let at = p % events.len();
        events.insert(at, json!({"type": "ping"}));
    }
    Built { events, expected }
}

fn run(messages: bool, limit: Option<usize>, events: &[Value]) -> (Vec<Ev>, Outcome, usize, State) {
    let mut state = State {
        messages,
        limit,
        ..State::default()
    };
    let (got, outcome, consumed) = drive(|v| state.push(v), events);
    let got = got
        .into_iter()
        .map(|e| match e {
            Ev::Usage(u) => Ev::Usage(Usage {
                reported_model: None,
                ..u
            }),
            e => e,
        })
        .collect();
    (got, outcome, consumed, state)
}

// ---- checks on any stream --------------------------------------------------

const KINDS: &[&str] = &[
    "message_start",
    "content_block_start",
    "content_block_delta",
    "content_block_stop",
    "message_delta",
    "message_stop",
    "ping",
    "error",
];

fn kind_of(event: &Value) -> &str {
    event["type"].as_str().unwrap_or("")
}

/// Invariants of whatever a driven stream produced, valid or not.
fn check_any(
    messages: bool,
    events: &[Value],
    got: &[Ev],
    outcome: &Outcome,
    consumed: usize,
    state: &mut State,
) -> std::result::Result<(), TestCaseError> {
    check_terminals(got)?;
    let seen = &events[..consumed];
    match outcome {
        Outcome::Error(e) => {
            let classified = *e != InferenceError::InvalidUpstream;
            if classified {
                prop_assert!(
                    messages && kind_of(&events[consumed - 1]) == "error",
                    "{e:?}"
                );
                prop_assert!(matches!(
                    e,
                    InferenceError::ContextLength
                        | InferenceError::Overloaded
                        | InferenceError::Busy
                ));
            }
        }
        Outcome::Eof => prop_assert!(!got.iter().any(|e| matches!(e, Ev::Usage(_) | Ev::Done))),
        Outcome::Done => {
            prop_assert_eq!(kind_of(&events[consumed - 1]), "message_stop");
            prop_assert!(got.iter().any(|e| matches!(e, Ev::Finish(_))));
        }
    }
    // Finish comes from a message_delta that carries a stop_reason.
    if got.iter().any(|e| matches!(e, Ev::Finish(_))) {
        prop_assert!(
            seen.iter()
                .any(|e| kind_of(e) == "message_delta" && e["delta"]["stop_reason"].is_string())
        );
    }
    // Everything emitted comes from what the upstream sent.
    let (mut strings, mut numbers) = (BTreeSet::new(), BTreeSet::new());
    for e in seen {
        leaves(e, &mut strings, &mut numbers);
    }
    let delta_outputs: BTreeSet<u64> = seen
        .iter()
        .filter(|e| kind_of(e) == "message_delta")
        .filter_map(|e| e["usage"]["output_tokens"].as_u64())
        .collect();
    let (mut started, mut args): (u32, Vec<String>) = (0, vec![]);
    for e in got {
        match e {
            Ev::Text(t) => prop_assert!(strings.contains(t), "invented text {t:?}"),
            Ev::ToolStart { index, id, name } => {
                prop_assert_eq!(*index, started, "tool indices are not contiguous");
                prop_assert!(started < 128);
                prop_assert!(strings.contains(id) && strings.contains(name));
                started += 1;
                args.push(String::new());
            }
            Ev::ToolArgs { index, args: a } => {
                prop_assert!(*index < started, "arguments for a tool never announced");
                prop_assert!(a == "{}" || strings.contains(a), "invented arguments {a:?}");
                args[*index as usize].push_str(a);
            }
            Ev::Block(v) => {
                prop_assert!(
                    messages && seen.contains(v),
                    "a preserved frame the upstream never sent"
                );
            }
            Ev::Usage(u) => {
                let known = |n: Option<u64>| n.is_none_or(|n| numbers.contains(&n));
                prop_assert!(known(u.input_tokens) && known(u.reasoning_tokens));
                // The start event's output count is preliminary and never used.
                prop_assert!(
                    u.output_tokens.is_none_or(|n| delta_outputs.contains(&n)),
                    "output {:?}",
                    u.output_tokens
                );
                if let Some(b) = u.billing {
                    prop_assert!(
                        known(b.cache_read_input_tokens) && known(b.cache_write_input_tokens)
                    );
                    prop_assert!(known(b.uncached_input_tokens));
                }
            }
            _ => {}
        }
    }
    if *outcome == Outcome::Done {
        // A tool block that ended delivered a JSON object.
        for a in &args {
            prop_assert!(
                serde_json::from_str::<Value>(a).is_ok_and(|v| v.is_object()),
                "arguments {a:?}"
            );
        }
        // Nothing is accepted after Done.
        for later in [
            json!({"type": "ping"}),
            json!({"type": "message_stop"}),
            json!({"type": "message_start", "message": {}}),
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
            json!({"type": "error", "error": {"type": "overloaded_error"}}),
        ] {
            prop_assert_eq!(
                state.push(later).err(),
                Some(InferenceError::InvalidUpstream)
            );
        }
    }
    Ok(())
}

// ---- properties ------------------------------------------------------------

proptest! {
    #![proptest_config(config(1500))]

    /// A valid stream (start, text, tool-use, and in Messages mode thinking
    /// blocks with their deltas, a stop reason with usage, a stop, pings
    /// anywhere) is reproduced exactly: text and argument fragments in order, an
    /// empty tool input delivered as `{}`, every native frame preserved in
    /// Messages mode, then one Finish, one Usage equal to the merged reported
    /// counts and one Done; and nothing is accepted afterwards.
    #[test]
    fn a_valid_stream_is_reproduced_exactly_and_terminates_once(spec in spec()) {
        let built = build(&spec);
        let (got, outcome, consumed, mut state) = run(spec.messages, None, &built.events);
        prop_assert_eq!(&outcome, &Outcome::Done);
        prop_assert_eq!(consumed, built.events.len());
        prop_assert_eq!(&got, &built.expected);
        check_any(spec.messages, &built.events, &got, &outcome, consumed, &mut state)?;
    }

    /// A stream cut anywhere before `message_stop` never completes: it ends at
    /// end-of-frames, emits no Usage or Done, and what it emitted is a prefix
    /// of the full stream's events.
    #[test]
    fn a_truncated_stream_never_finishes(spec in spec(), cut in any::<usize>()) {
        let built = build(&spec);
        let stop_at = built.events.iter().position(|e| kind_of(e) == "message_stop").unwrap();
        let cut = cut % (stop_at + 1);
        let (got, outcome, _, _) = run(spec.messages, None, &built.events[..cut]);
        prop_assert_eq!(&outcome, &Outcome::Eof);
        prop_assert!(!got.iter().any(|e| matches!(e, Ev::Usage(_) | Ev::Done)));
        prop_assert_eq!(&got[..], &built.expected[..got.len()]);
    }

    /// A valid stream with events dropped, duplicated, swapped or truncated,
    /// fields of the wrong type, huge, negative, empty and oversized values,
    /// unknown fields and unknown event types: never a panic, only the
    /// documented errors, at most one Finish/Usage/Done in order, Done only
    /// after a stop reason and a `message_stop`, nothing accepted after Done,
    /// and no text, tool call or count the upstream did not send.
    #[test]
    fn a_mutated_stream_is_refused_or_stays_consistent(
        spec in spec(),
        mutations in prop::collection::vec(mutation(KINDS), 1..6),
        limit in prop::option::of(1..400_usize),
    ) {
        let mut events = build(&spec).events;
        apply_all(&mut events, &mutations);
        let (got, outcome, consumed, mut state) = run(spec.messages, limit, &events);
        check_any(spec.messages, &events, &got, &outcome, consumed, &mut state)?;
    }

    /// Arbitrary JSON events after a plausible start: no panic, documented
    /// errors only.
    #[test]
    fn arbitrary_events_are_refused_without_panic(
        events in prop::collection::vec(arb_json(), 0..12),
        started in any::<bool>(),
        messages in any::<bool>(),
    ) {
        let mut all = vec![];
        if started {
            all.push(build(&Spec { messages, blocks: vec![], stop: 0, start: None, delta: None, pings: vec![] }).events.remove(0));
        }
        all.extend(events);
        let (got, outcome, consumed, mut state) = run(messages, None, &all);
        check_any(messages, &all, &got, &outcome, consumed, &mut state)?;
    }

    /// A cumulative counter that goes down, or any count past `i64::MAX`, is
    /// refused when `message_delta` arrives, before any Finish.
    #[test]
    fn a_decreasing_or_oversized_count_is_refused(
        input in 1..1_000_000_u64,
        drop_by in 1..1000_u64,
        huge in (i64::MAX as u64 + 1)..=u64::MAX,
        field in prop::sample::select(vec!["input_tokens", "cache_read_input_tokens", "cache_creation_input_tokens", "output_tokens"]),
        oversize in any::<bool>(),
    ) {
        let low = input.saturating_sub(drop_by.min(input));
        let mut start_usage = json!({"input_tokens": input, "cache_read_input_tokens": input, "cache_creation_input_tokens": input});
        let mut delta_usage = json!({});
        if oversize {
            delta_usage[field] = json!(huge);
        } else if field == "output_tokens" {
            // The start's output is preliminary, so a lower delta is not a decrease.
            start_usage["output_tokens"] = json!(input);
            delta_usage["output_tokens"] = json!(low);
        } else {
            delta_usage[field] = json!(low);
        }
        let events = [
            json!({"type": "message_start", "message": {"id": "m", "type": "message", "role": "assistant", "content": [], "stop_reason": null, "usage": start_usage}}),
            json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": delta_usage}),
            json!({"type": "message_stop"}),
        ];
        let (got, outcome, _, _) = run(false, None, &events);
        if oversize || field != "output_tokens" {
            prop_assert_eq!(outcome, Outcome::Error(InferenceError::InvalidUpstream));
            prop_assert!(!got.iter().any(|e| matches!(e, Ev::Finish(_) | Ev::Usage(_) | Ev::Done)));
        } else {
            prop_assert_eq!(outcome, Outcome::Done);
            prop_assert!(matches!(&got[1], Ev::Usage(u) if u.output_tokens == Some(low)));
        }
    }

    /// Tool-argument fragments beyond the response limit are refused at the
    /// fragment that crosses it; what was delivered before never exceeds the
    /// limit and is never cut short.
    #[test]
    fn tool_arguments_beyond_the_limit_are_refused_not_truncated(
        fragments in prop::collection::vec("[a-z\" :,]{0,30}", 1..8),
        limit in 0..120_usize,
    ) {
        let mut events = vec![
            json!({"type": "message_start", "message": {"id": "m", "type": "message", "role": "assistant", "content": [], "stop_reason": null}}),
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "tool_use", "id": "t", "name": "n", "input": {}}}),
        ];
        for f in &fragments {
            events.push(json!({"type": "content_block_delta", "index": 0, "delta": {"type": "input_json_delta", "partial_json": f}}));
        }
        let (got, outcome, _, _) = run(false, Some(limit), &events);
        let delivered: usize = got.iter().map(|e| if let Ev::ToolArgs { args, .. } = e { args.len() } else { 0 }).sum();
        let total: usize = fragments.iter().map(String::len).sum();
        prop_assert!(delivered <= limit);
        if total <= limit {
            prop_assert_eq!(outcome, Outcome::Eof);
            prop_assert_eq!(delivered, total);
        } else {
            prop_assert_eq!(outcome, Outcome::Error(InferenceError::InvalidUpstream));
        }
    }
}

// ---- limits and named cases ------------------------------------------------

fn blocks_stream(blocks: usize, tool: bool) -> Vec<Value> {
    let mut events = vec![
        json!({"type": "message_start", "message": {"id": "m", "type": "message", "role": "assistant", "content": [], "stop_reason": null}}),
    ];
    for i in 0..blocks {
        let block = if tool {
            json!({"type": "tool_use", "id": format!("t{i}"), "name": "n", "input": {}})
        } else {
            json!({"type": "text", "text": "x"})
        };
        events.push(json!({"type": "content_block_start", "index": i, "content_block": block}));
        events.push(json!({"type": "content_block_stop", "index": i}));
    }
    events.push(json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}}));
    events.push(json!({"type": "message_stop"}));
    events
}

/// 256 blocks and 128 tool calls are accepted whole; one more of either is
/// refused at the block that would exceed it, never truncated.
#[test]
fn block_and_tool_limits_refuse_the_next_unit() {
    for messages in [false, true] {
        let (got, outcome, _, _) = run(messages, None, &blocks_stream(256, false));
        assert_eq!(outcome, Outcome::Done, "256 blocks");
        assert_eq!(got.iter().filter(|e| matches!(e, Ev::Text(_))).count(), 256);
        let (got, outcome, _, _) = run(messages, None, &blocks_stream(257, false));
        assert_eq!(
            outcome,
            Outcome::Error(InferenceError::InvalidUpstream),
            "257 blocks"
        );
        assert_eq!(got.iter().filter(|e| matches!(e, Ev::Text(_))).count(), 256);
        assert!(!got.iter().any(|e| matches!(e, Ev::Finish(_) | Ev::Done)));

        let (got, outcome, _, _) = run(messages, None, &blocks_stream(128, true));
        assert_eq!(outcome, Outcome::Done, "128 tools");
        assert_eq!(
            got.iter()
                .filter(|e| matches!(e, Ev::ToolStart { .. }))
                .count(),
            128
        );
        let (got, outcome, _, _) = run(messages, None, &blocks_stream(129, true));
        assert_eq!(
            outcome,
            Outcome::Error(InferenceError::InvalidUpstream),
            "129 tools"
        );
        assert_eq!(
            got.iter()
                .filter(|e| matches!(e, Ev::ToolStart { .. }))
                .count(),
            128
        );
    }
}

/// Upstream `error` events: a Messages stream classifies the three documented
/// envelopes, everything else (and every non-Messages stream) is the one
/// class; the upstream's text is never reflected.
#[test]
fn error_events_are_classified_without_reflecting_text() {
    let cases = [
        (
            json!({"type": "overloaded_error", "message": "secret"}),
            InferenceError::Overloaded,
        ),
        (
            json!({"type": "rate_limit_error", "message": "secret"}),
            InferenceError::Busy,
        ),
        (
            json!({"type": "invalid_request_error", "message": "prompt is too long: 250000 tokens"}),
            InferenceError::ContextLength,
        ),
        (
            json!({"type": "invalid_request_error", "message": "Client said prompt is too long"}),
            InferenceError::InvalidUpstream,
        ),
        (
            json!({"type": "api_error", "message": "secret"}),
            InferenceError::InvalidUpstream,
        ),
        (
            json!({"message": "secret"}),
            InferenceError::InvalidUpstream,
        ),
        (json!("overloaded_error"), InferenceError::InvalidUpstream),
    ];
    for (error, class) in cases {
        let event = json!({"type": "error", "error": error});
        let (_, outcome, _, _) = run(true, None, std::slice::from_ref(&event));
        assert_eq!(outcome, Outcome::Error(class), "{event}");
        let (_, outcome, _, _) = run(false, None, std::slice::from_ref(&event));
        assert_eq!(
            outcome,
            Outcome::Error(InferenceError::InvalidUpstream),
            "{event}"
        );
    }
}
