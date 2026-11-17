//! Property tests of the Bedrock ConverseStream parser, `stream::State` (#593).
//!
//! The contract these tests state, read from the code and from `decode`'s loop:
//!
//! * `push` never panics, and its only error is `InvalidUpstream`: a frame
//!   that arrived whole and decodes but means nothing here is the provider's
//!   protocol violation. Transport failures (reset, truncation, a corrupt
//!   frame) never reach the parser: they are classified where they arise, from
//!   `decode`'s receive error.
//! * Termination. `Finish` comes from the one `messageStop` that follows
//!   `messageStart` with every block closed and a stop reason that agrees with
//!   the tool calls; `Usage` from the one `metadata` after it. Bedrock has no
//!   in-band `Done`: `decode` yields `Done` only when the frames end cleanly and
//!   `State::end` accepts, which needs start, stop and metadata. Any other end
//!   is a truncation, `UpstreamUnavailable`, never a success, and the parser
//!   fabricates nothing for a truncated stream. A second `messageStop`, `metadata`, a block after
//!   the stop or a second start is refused.
//! * Usage is exactly what `metadata` reported (negative and over-range counts
//!   are refused), never a fabricated zero. Only the directly reported fields
//!   (input, output, cache read, cache write) are compared, plus
//!   `total >= input`; the derived fields (uncached input, the TTL
//!   allocations from `cacheDetails`) are `providers/metering.rs`'s rules
//!   (`docs/cache-pricing.md`) and belong to its tests.
//! * A failing generated case must be pasted into a named regression test:
//!   failure seeds are not persisted (`failure_persistence: None`).
//! * Content. Text deltas and tool-input fragments are delivered in order with
//!   contiguous tool indices; a tool block's fragments concatenate to a JSON
//!   object. Limits refuse rather than truncate: 256 blocks, 128 tools, and the
//!   response limit on the bytes streamed (a refused push returns none of its
//!   events).
use super::*;
use crate::{
    billing::BillingUsage,
    proptest_support_tests::{Ev, Outcome, arb_json, check_terminals, config, flatten, split_text},
};
use proptest::prelude::*;
use serde_json::json;
use std::collections::BTreeSet;

// ---- the wire events --------------------------------------------------------

#[derive(Clone, Debug)]
struct U {
    input: i32,
    output: i32,
    total: i32,
    read: Option<i32>,
    write: Option<i32>,
    details: Option<Vec<(String, i32)>>,
}

#[derive(Clone, Debug)]
enum W {
    Start {
        assistant: bool,
    },
    BlockStart {
        index: i32,
        tool: Option<(String, String)>,
    },
    Text {
        index: i32,
        text: String,
    },
    ToolInput {
        index: i32,
        input: String,
    },
    Stop {
        index: i32,
    },
    MessageStop {
        reason: String,
    },
    Metadata {
        usage: Option<U>,
    },
}

fn token_usage(u: &U) -> aws::TokenUsage {
    aws::TokenUsage::builder()
        .input_tokens(u.input)
        .output_tokens(u.output)
        .total_tokens(u.total)
        .set_cache_read_input_tokens(u.read)
        .set_cache_write_input_tokens(u.write)
        .set_cache_details(u.details.as_ref().map(|d| {
            d.iter()
                .map(|(ttl, n)| {
                    aws::CacheDetail::builder()
                        .ttl(aws::CacheTtl::from(ttl.as_str()))
                        .input_tokens(*n)
                        .build()
                        .unwrap()
                })
                .collect()
        }))
        .build()
        .unwrap()
}

fn to_aws(w: &W) -> aws::ConverseStreamOutput {
    use aws::ConverseStreamOutput as E;
    match w {
        W::Start { assistant } => E::MessageStart(
            aws::MessageStartEvent::builder()
                .role(if *assistant {
                    aws::ConversationRole::Assistant
                } else {
                    aws::ConversationRole::User
                })
                .build()
                .unwrap(),
        ),
        W::BlockStart { index, tool } => {
            let mut b = aws::ContentBlockStartEvent::builder().content_block_index(*index);
            if let Some((id, name)) = tool {
                b = b.start(aws::ContentBlockStart::ToolUse(
                    aws::ToolUseBlockStart::builder()
                        .tool_use_id(id)
                        .name(name)
                        .build()
                        .unwrap(),
                ));
            }
            E::ContentBlockStart(b.build().unwrap())
        }
        W::Text { index, text } => E::ContentBlockDelta(
            aws::ContentBlockDeltaEvent::builder()
                .content_block_index(*index)
                .delta(aws::ContentBlockDelta::Text(text.clone()))
                .build()
                .unwrap(),
        ),
        W::ToolInput { index, input } => E::ContentBlockDelta(
            aws::ContentBlockDeltaEvent::builder()
                .content_block_index(*index)
                .delta(aws::ContentBlockDelta::ToolUse(
                    aws::ToolUseBlockDelta::builder()
                        .input(input)
                        .build()
                        .unwrap(),
                ))
                .build()
                .unwrap(),
        ),
        W::Stop { index } => E::ContentBlockStop(
            aws::ContentBlockStopEvent::builder()
                .content_block_index(*index)
                .build()
                .unwrap(),
        ),
        W::MessageStop { reason } => E::MessageStop(
            aws::MessageStopEvent::builder()
                .stop_reason(aws::StopReason::from(reason.as_str()))
                .build()
                .unwrap(),
        ),
        W::Metadata { usage } => {
            let mut b = aws::ConverseStreamMetadataEvent::builder();
            if let Some(u) = usage {
                b = b.usage(token_usage(u));
            }
            E::Metadata(b.build())
        }
    }
}

// ---- spec ------------------------------------------------------------------

#[derive(Clone, Debug)]
enum Blk {
    Text {
        explicit: bool,
        pieces: Vec<String>,
    },
    Tool {
        name: String,
        args: Value,
        cuts: Vec<usize>,
    },
}

#[derive(Clone, Debug)]
struct Spec {
    blocks: Vec<(Blk, u8)>,
    reason: usize,
    usage: Option<UsageSpec>,
}

#[derive(Clone, Debug)]
struct UsageSpec {
    input: i32,
    output: i32,
    read: Option<i32>,
    write: Option<i32>,
    details: Option<(Option<i32>, Option<i32>)>,
}

const REASONS: [(&str, FinishReason); 5] = [
    ("end_turn", FinishReason::Stop),
    ("stop_sequence", FinishReason::Stop),
    ("max_tokens", FinishReason::Length),
    ("guardrail_intervened", FinishReason::ContentFilter),
    ("content_filtered", FinishReason::ContentFilter),
];

fn text() -> impl Strategy<Value = String> {
    prop_oneof![
        3 => "[ -~\n]{0,12}",
        1 => prop::collection::vec(any::<char>(), 0..6).prop_map(|v| v.into_iter().collect::<String>()),
    ]
}

fn blk() -> impl Strategy<Value = (Blk, u8)> {
    (
        prop_oneof![
            3 => (any::<bool>(), prop::collection::vec(text(), 0..4))
                .prop_map(|(explicit, pieces)| Blk::Text { explicit, pieces }),
            2 => ("[A-Za-z0-9_-]{1,12}", arb_json(), prop::collection::vec(any::<usize>(), 0..4))
                .prop_map(|(name, args, cuts)| Blk::Tool {
                    name,
                    args: if args.is_object() { args } else { json!({ "value": args }) },
                    cuts,
                }),
        ],
        0..3_u8,
    )
}

fn count() -> impl Strategy<Value = i32> {
    prop_oneof![3 => 0..5000_i32, 1 => 0..=(i32::MAX / 4)]
}

fn usage_spec() -> impl Strategy<Value = UsageSpec> {
    (
        count(),
        count(),
        prop::option::of(count()),
        prop::option::of(count()),
        prop::option::of((prop::option::of(count()), prop::option::of(count()))),
    )
        .prop_map(|(input, output, read, write, details)| UsageSpec {
            input,
            output,
            read,
            write,
            details,
        })
}

fn spec() -> impl Strategy<Value = Spec> {
    (
        prop::collection::vec(blk(), 0..6),
        any::<usize>(),
        prop::option::of(usage_spec()),
    )
        .prop_map(|(blocks, reason, usage)| Spec {
            blocks,
            reason,
            usage,
        })
}

/// The wire usage for a spec: cache details, when given, are the complete TTL
/// allocation, so the cache-write total is their sum.
fn wire_usage(u: &UsageSpec) -> U {
    let details = u.details.map(|(five, hour)| {
        let mut d = vec![];
        if let Some(a) = five {
            d.push(("5m".to_owned(), a));
        }
        if let Some(b) = hour {
            d.push(("1h".to_owned(), b));
        }
        d
    });
    let write = match u.details {
        Some((a, b)) => u.write.map(|_| a.unwrap_or(0) + b.unwrap_or(0)),
        None => u.write,
    };
    U {
        input: u.input,
        output: u.output,
        total: u.input + u.output,
        read: u.read,
        write,
        details,
    }
}

/// The directly reported fields only (see the module doc).
fn project(u: &Usage) -> Usage {
    Usage {
        input_tokens: u.input_tokens,
        output_tokens: u.output_tokens,
        billing: u.billing.map(|b| {
            if let Some(i) = u.input_tokens {
                assert!(
                    b.total_input_tokens.is_none_or(|t| t >= i),
                    "total below input"
                );
            }
            BillingUsage {
                cache_read_input_tokens: b.cache_read_input_tokens,
                cache_write_input_tokens: b.cache_write_input_tokens,
                ..BillingUsage::default()
            }
        }),
        ..Usage::default()
    }
}

fn expected_usage(u: &UsageSpec) -> Usage {
    let w = wire_usage(u);
    Usage {
        input_tokens: Some(u.input as u64),
        output_tokens: Some(u.output as u64),
        billing: Some(BillingUsage {
            cache_read_input_tokens: w.read.map(|n| n as u64),
            cache_write_input_tokens: w.write.map(|n| n as u64),
            ..BillingUsage::default()
        }),
        ..Usage::default()
    }
}

struct Built {
    events: Vec<W>,
    expected: Vec<Ev>,
    /// Bytes the parser counts against the response limit.
    size: usize,
}

fn build(spec: &Spec) -> Built {
    let mut events = vec![W::Start { assistant: true }];
    let mut expected = vec![];
    let (mut index, mut tools, mut size) = (0_i32, 0_u32, 0_usize);
    for (blk, gap) in &spec.blocks {
        index += i32::from(*gap);
        match blk {
            Blk::Text { explicit, pieces } => {
                if *explicit {
                    events.push(W::BlockStart { index, tool: None });
                }
                for p in pieces {
                    events.push(W::Text {
                        index,
                        text: p.clone(),
                    });
                    expected.push(Ev::Text(p.clone()));
                    size += p.len();
                }
                if *explicit || !pieces.is_empty() {
                    events.push(W::Stop { index });
                }
            }
            Blk::Tool { name, args, cuts } => {
                let id = format!("tool_{tools}");
                size += id.len() + name.len();
                events.push(W::BlockStart {
                    index,
                    tool: Some((id.clone(), name.clone())),
                });
                expected.push(Ev::ToolStart {
                    index: tools,
                    id,
                    name: name.clone(),
                });
                for p in split_text(&args.to_string(), cuts) {
                    size += p.len();
                    expected.push(Ev::ToolArgs {
                        index: tools,
                        args: p.clone(),
                    });
                    events.push(W::ToolInput { index, input: p });
                }
                events.push(W::Stop { index });
                tools += 1;
            }
        }
        index += 1;
    }
    let (reason, finish) = if tools > 0 {
        ("tool_use", FinishReason::ToolCalls)
    } else {
        REASONS[spec.reason % REASONS.len()]
    };
    events.push(W::MessageStop {
        reason: reason.into(),
    });
    expected.push(Ev::Finish(finish));
    events.push(W::Metadata {
        usage: spec.usage.as_ref().map(wire_usage),
    });
    if let Some(u) = &spec.usage {
        expected.push(Ev::Usage(expected_usage(u)));
    }
    Built {
        events,
        expected,
        size,
    }
}

/// Feed events to the parser the way `decode` does: every push in order, the
/// first error ends the stream, and when the frames run out `Done` is yielded
/// only if `end()` accepts (otherwise the stream is truncated and ends in
/// `UpstreamUnavailable`, reported as `Eof`).
fn run(events: &[W], limit: Option<usize>) -> (Vec<Ev>, Outcome, usize) {
    let mut state = State {
        limit,
        ..State::default()
    };
    let mut out = vec![];
    for (i, w) in events.iter().enumerate() {
        match state.push(to_aws(w)) {
            Err(e) => return (out, Outcome::Error(e), i + 1),
            Ok(list) => out.extend(list.into_iter().flat_map(flatten).map(|e| match e {
                Ev::Usage(u) => Ev::Usage(project(&u)),
                e => e,
            })),
        }
    }
    let outcome = match state.end() {
        Ok(()) => Outcome::Done,
        Err(e) => {
            // Frames that ran out before the terminal sequence: a truncation.
            assert_eq!(e, InferenceError::UpstreamUnavailable);
            Outcome::Eof
        }
    };
    (out, outcome, events.len())
}

// ---- mutations -------------------------------------------------------------

fn arb_u() -> impl Strategy<Value = U> {
    let n = prop_oneof![
        4 => 0..1000_i32,
        1 => Just(-1_i32),
        1 => Just(i32::MAX),
        1 => Just(i32::MIN),
        1 => any::<i32>(),
    ];
    (
        n.clone(),
        n.clone(),
        n.clone(),
        prop::option::of(n.clone()),
        prop::option::of(n.clone()),
        prop::option::of(prop::collection::vec(
            (
                prop::sample::select(vec!["5m", "1h", "1d", "", "5M"]).prop_map(str::to_owned),
                n,
            ),
            0..4,
        )),
    )
        .prop_map(|(input, output, total, read, write, details)| U {
            input,
            output,
            total,
            read,
            write,
            details,
        })
}

fn index() -> impl Strategy<Value = i32> {
    prop::sample::select(vec![-1, 0, 1, 2, 3, 255, 256, 257, i32::MAX, i32::MIN])
}

fn arb_w() -> impl Strategy<Value = W> {
    let ident = prop_oneof![
        4 => "[A-Za-z0-9_-]{1,10}",
        1 => Just(String::new()),
        1 => Just("has space".to_owned()),
        1 => Just("x".repeat(65)),
    ];
    prop_oneof![
        any::<bool>().prop_map(|assistant| W::Start { assistant }),
        (index(), prop::option::of((ident.clone(), ident)))
            .prop_map(|(index, tool)| W::BlockStart { index, tool }),
        (index(), text()).prop_map(|(index, text)| W::Text { index, text }),
        (index(), text()).prop_map(|(index, input)| W::ToolInput { index, input }),
        index().prop_map(|index| W::Stop { index }),
        prop::sample::select(vec![
            "end_turn",
            "tool_use",
            "max_tokens",
            "content_filtered",
            "other",
            ""
        ])
        .prop_map(|r| W::MessageStop { reason: r.into() }),
        prop::option::of(arb_u()).prop_map(|usage| W::Metadata { usage }),
    ]
}

#[derive(Clone, Debug)]
enum Edit {
    Drop(usize),
    Duplicate(usize),
    Swap(usize, usize),
    Truncate(usize),
    Index(usize, i32),
    Replace(usize, W),
}

fn edit() -> impl Strategy<Value = Edit> {
    prop_oneof![
        any::<usize>().prop_map(Edit::Drop),
        any::<usize>().prop_map(Edit::Duplicate),
        (any::<usize>(), any::<usize>()).prop_map(|(a, b)| Edit::Swap(a, b)),
        any::<usize>().prop_map(Edit::Truncate),
        (any::<usize>(), index()).prop_map(|(i, n)| Edit::Index(i, n)),
        (any::<usize>(), arb_w()).prop_map(|(i, w)| Edit::Replace(i, w)),
    ]
}

fn apply(events: &mut Vec<W>, e: &Edit) {
    let n = events.len();
    if n == 0 {
        return;
    }
    match e {
        Edit::Drop(i) => {
            events.remove(i % n);
        }
        Edit::Duplicate(i) => {
            let w = events[i % n].clone();
            events.insert(i % n, w);
        }
        Edit::Swap(a, b) => events.swap(a % n, b % n),
        Edit::Truncate(len) => events.truncate(len % (n + 1)),
        Edit::Index(i, to) => match &mut events[i % n] {
            W::BlockStart { index, .. }
            | W::Text { index, .. }
            | W::ToolInput { index, .. }
            | W::Stop { index } => *index = *to,
            _ => {}
        },
        Edit::Replace(i, w) => events[i % n] = w.clone(),
    }
}

// ---- checks on any stream --------------------------------------------------

fn check_any(
    events: &[W],
    got: &[Ev],
    outcome: &Outcome,
    consumed: usize,
) -> std::result::Result<(), TestCaseError> {
    check_terminals(got)?;
    prop_assert!(
        !got.iter().any(|e| matches!(e, Ev::Done | Ev::Block(_))),
        "State yields no Done"
    );
    let seen = &events[..consumed];
    if let Outcome::Error(e) = outcome {
        prop_assert_eq!(*e, InferenceError::InvalidUpstream);
    }
    let position = |f: fn(&W) -> bool| seen.iter().position(f);
    let has = |f: fn(&W) -> bool| seen.iter().any(f);
    // Done needs the whole terminal sequence, in order, accepted.
    if *outcome == Outcome::Done {
        let start = position(|w| matches!(w, W::Start { assistant: true }));
        let stop = position(|w| matches!(w, W::MessageStop { .. }));
        let meta = position(|w| matches!(w, W::Metadata { .. }));
        prop_assert!(
            matches!((start, stop, meta), (Some(a), Some(b), Some(c)) if a < b && b < c),
            "{seen:?}"
        );
        prop_assert!(got.iter().any(|e| matches!(e, Ev::Finish(_))));
    }
    // Finish needs a stop, Usage a metadata after it.
    if got.iter().any(|e| matches!(e, Ev::Finish(_))) {
        prop_assert!(
            has(|w| matches!(w, W::MessageStop { .. })),
            "Finish without a stop"
        );
    }
    if got.iter().any(|e| matches!(e, Ev::Usage(_))) {
        prop_assert!(
            has(|w| matches!(w, W::Metadata { usage: Some(_) })),
            "Usage without metadata"
        );
        prop_assert!(
            got.iter().any(|e| matches!(e, Ev::Finish(_))),
            "Usage before Finish"
        );
    }
    // Nothing invented.
    let texts: BTreeSet<&str> = seen
        .iter()
        .filter_map(|w| match w {
            W::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    let inputs: BTreeSet<&str> = seen
        .iter()
        .filter_map(|w| match w {
            W::ToolInput { input, .. } => Some(input.as_str()),
            _ => None,
        })
        .collect();
    let tools: BTreeSet<(&str, &str)> = seen
        .iter()
        .filter_map(|w| match w {
            W::BlockStart {
                tool: Some((i, n)), ..
            } => Some((i.as_str(), n.as_str())),
            _ => None,
        })
        .collect();
    let mut started = 0_u32;
    let mut args: Vec<String> = vec![];
    for e in got {
        match e {
            Ev::Text(t) => prop_assert!(texts.contains(t.as_str()), "invented text {t:?}"),
            Ev::ToolStart { index, id, name } => {
                prop_assert_eq!(*index, started, "tool indices are not contiguous");
                prop_assert!(started < 128);
                prop_assert!(tools.contains(&(id.as_str(), name.as_str())));
                started += 1;
                args.push(String::new());
            }
            Ev::ToolArgs { index, args: a } => {
                prop_assert!(*index < started, "arguments for a tool never announced");
                prop_assert!(inputs.contains(a.as_str()), "invented arguments {a:?}");
                args[*index as usize].push_str(a);
            }
            Ev::Usage(u) => {
                let reported: Vec<&U> = seen
                    .iter()
                    .filter_map(|w| match w {
                        W::Metadata { usage: Some(u) } => Some(u),
                        _ => None,
                    })
                    .collect();
                prop_assert!(
                    reported.iter().any(|r| {
                        u.input_tokens == Some(r.input as u64)
                            && u.output_tokens == Some(r.output as u64)
                    }),
                    "usage not reported by the upstream: {u:?}"
                );
                let b = u.billing.unwrap();
                prop_assert!(
                    reported.iter().any(|r| {
                        b.cache_read_input_tokens == r.read.map(|n| n as u64)
                            && b.cache_write_input_tokens == r.write.map(|n| n as u64)
                    }),
                    "cache counts not reported by the upstream"
                );
            }
            _ => {}
        }
    }
    // A tool block that was closed delivered a JSON object.
    if has(|w| matches!(w, W::Stop { .. })) && *outcome == Outcome::Done {
        for a in &args {
            prop_assert!(
                serde_json::from_str::<Value>(a).is_ok_and(|v| v.is_object()),
                "arguments {a:?}"
            );
        }
    }
    Ok(())
}

// ---- properties ------------------------------------------------------------

proptest! {
    #![proptest_config(config(1500))]

    /// A valid stream (start, text blocks started explicitly or by their first
    /// delta, tool blocks with their id, name and JSON input, a stop reason that
    /// agrees with the tool calls, metadata with or without usage) is
    /// reproduced exactly: text and argument fragments in order with contiguous
    /// tool indices, one Finish, one Usage equal to the reported counts (cache
    /// details round-trip; absent usage emits none), and `end()` accepts.
    #[test]
    fn a_valid_stream_is_reproduced_exactly_and_terminates_once(spec in spec()) {
        let built = build(&spec);
        let (got, outcome, consumed) = run(&built.events, None);
        prop_assert_eq!(&outcome, &Outcome::Done);
        prop_assert_eq!(consumed, built.events.len());
        prop_assert_eq!(&got, &built.expected);
        check_any(&built.events, &got, &outcome, consumed)?;
    }

    /// A stream cut anywhere before the end (including just before the
    /// metadata) is not accepted by `end()`, and what it emitted is a prefix of
    /// the full stream's events.
    #[test]
    fn a_truncated_stream_never_finishes(spec in spec(), cut in any::<usize>()) {
        let built = build(&spec);
        let cut = cut % built.events.len();
        let (got, outcome, _) = run(&built.events[..cut], None);
        prop_assert_eq!(&outcome, &Outcome::Eof);
        prop_assert_eq!(&got[..], &built.expected[..got.len()]);
    }

    /// A valid stream with events dropped, duplicated, swapped, truncated,
    /// re-indexed or replaced by any other kind of event: never a panic, only
    /// `InvalidUpstream`, one Finish and one Usage in order, `end()` only after
    /// start, stop and metadata, and no text, tool call or count the upstream
    /// did not send.
    #[test]
    fn a_mutated_stream_is_refused_or_stays_consistent(
        spec in spec(),
        edits in prop::collection::vec(edit(), 1..6),
        limit in prop::option::of(1..200_usize),
    ) {
        let mut events = build(&spec).events;
        for e in &edits {
            apply(&mut events, e);
        }
        let (got, outcome, consumed) = run(&events, limit);
        check_any(&events, &got, &outcome, consumed)?;
    }

    /// Arbitrary events: no panic, only `InvalidUpstream`.
    #[test]
    fn arbitrary_events_are_refused_without_panic(
        events in prop::collection::vec(arb_w(), 0..14),
        started in any::<bool>(),
    ) {
        let mut all = if started { vec![W::Start { assistant: true }] } else { vec![] };
        all.extend(events);
        let (got, outcome, consumed) = run(&all, None);
        check_any(&all, &got, &outcome, consumed)?;
    }

    /// The streamed-byte limit: a valid stream is accepted whole when it fits
    /// and refused when it does not; what was delivered never exceeds the limit
    /// (a refused push returns none of its events).
    #[test]
    fn the_limit_refuses_rather_than_truncates(spec in spec(), limit in 0..300_usize) {
        let built = build(&spec);
        let (got, outcome, _) = run(&built.events, Some(limit));
        let delivered: usize = got.iter().map(|e| match e {
            Ev::Text(t) => t.len(),
            Ev::ToolArgs { args, .. } => args.len(),
            Ev::ToolStart { id, name, .. } => id.len() + name.len(),
            _ => 0,
        }).sum();
        prop_assert!(delivered <= limit);
        if built.size <= limit {
            prop_assert_eq!(&outcome, &Outcome::Done);
            prop_assert_eq!(&got, &built.expected);
        } else {
            prop_assert_eq!(&outcome, &Outcome::Error(InferenceError::InvalidUpstream));
        }
    }

    /// The byte limit is exact at the boundary: a valid stream whose counted
    /// size is `S` is accepted whole under limit `S` and refused under `S - 1`.
    #[test]
    fn the_limit_is_exact_at_the_boundary(spec in spec()) {
        let built = build(&spec);
        let (got, outcome, _) = run(&built.events, Some(built.size));
        prop_assert_eq!(&outcome, &Outcome::Done);
        prop_assert_eq!(&got, &built.expected);
        if built.size > 0 {
            let (got, outcome, _) = run(&built.events, Some(built.size - 1));
            prop_assert_eq!(&outcome, &Outcome::Error(InferenceError::InvalidUpstream));
            prop_assert!(!got.iter().any(|e| matches!(e, Ev::Finish(_) | Ev::Usage(_))));
        }
    }

    /// Reported counts are taken as sent: a negative or past-`i32` count is
    /// refused; consistent counts come back exactly; the usage is never filled
    /// in with zeros.
    #[test]
    fn metadata_usage_is_taken_as_sent_or_refused(usage in arb_u()) {
        let events = [
            W::Start { assistant: true },
            W::MessageStop { reason: "end_turn".into() },
            W::Metadata { usage: Some(usage.clone()) },
        ];
        let (got, outcome, _) = run(&events, None);
        let negative = [usage.input, usage.output, usage.total].iter().any(|n| *n < 0)
            || usage.read.is_some_and(|n| n < 0)
            || usage.write.is_some_and(|n| n < 0)
            || usage.details.as_ref().is_some_and(|d| d.iter().any(|(_, n)| *n < 0));
        if negative {
            prop_assert_eq!(&outcome, &Outcome::Error(InferenceError::InvalidUpstream));
            prop_assert!(!got.iter().any(|e| matches!(e, Ev::Usage(_))));
        }
        for e in &got {
            if let Ev::Usage(u) = e {
                prop_assert_eq!(u.input_tokens, Some(usage.input as u64));
                prop_assert_eq!(u.output_tokens, Some(usage.output as u64));
                let b = u.billing.unwrap();
                prop_assert_eq!(b.cache_read_input_tokens, usage.read.map(|n| n as u64));
                prop_assert_eq!(b.cache_write_input_tokens, usage.write.map(|n| n as u64));
            }
        }
    }
}

// ---- limits and named cases ------------------------------------------------

fn blocks_stream(blocks: usize, tool: bool) -> Vec<W> {
    let mut events = vec![W::Start { assistant: true }];
    for i in 0..blocks as i32 {
        events.push(W::BlockStart {
            index: i,
            tool: tool.then(|| (format!("t{i}"), "n".into())),
        });
        if tool {
            events.push(W::ToolInput {
                index: i,
                input: "{}".into(),
            });
        }
        events.push(W::Stop { index: i });
    }
    events.push(W::MessageStop {
        reason: if tool { "tool_use" } else { "end_turn" }.into(),
    });
    events.push(W::Metadata { usage: None });
    events
}

/// 256 blocks and 128 tools are accepted whole; one more of either is refused
/// at the block that would exceed it, never truncated.
#[test]
fn block_and_tool_limits_refuse_the_next_unit() {
    let (_, outcome, _) = run(&blocks_stream(MAX_BLOCKS, false), None);
    assert_eq!(outcome, Outcome::Done, "{MAX_BLOCKS} blocks");
    let (got, outcome, _) = run(&blocks_stream(MAX_BLOCKS + 1, false), None);
    assert_eq!(outcome, Outcome::Error(InferenceError::InvalidUpstream));
    assert!(!got.iter().any(|e| matches!(e, Ev::Finish(_))));

    let (got, outcome, _) = run(&blocks_stream(MAX_TOOLS, true), None);
    assert_eq!(outcome, Outcome::Done, "{MAX_TOOLS} tools");
    assert_eq!(
        got.iter()
            .filter(|e| matches!(e, Ev::ToolStart { .. }))
            .count(),
        MAX_TOOLS
    );
    let (got, outcome, _) = run(&blocks_stream(MAX_TOOLS + 1, true), None);
    assert_eq!(outcome, Outcome::Error(InferenceError::InvalidUpstream));
    assert_eq!(
        got.iter()
            .filter(|e| matches!(e, Ev::ToolStart { .. }))
            .count(),
        MAX_TOOLS
    );
}

/// A tool block that closes without a JSON-object input is refused, including
/// an empty input (unlike the Anthropic parser, no `{}` is invented here).
#[test]
fn a_tool_input_that_is_not_an_object_is_refused_at_its_stop() {
    for input in ["", "[1]", "\"s\"", "1", "null", "{", "{} {}"] {
        let events = [
            W::Start { assistant: true },
            W::BlockStart {
                index: 0,
                tool: Some(("t".into(), "n".into())),
            },
            W::ToolInput {
                index: 0,
                input: input.into(),
            },
            W::Stop { index: 0 },
            W::MessageStop {
                reason: "tool_use".into(),
            },
            W::Metadata { usage: None },
        ];
        let (got, outcome, _) = run(&events, None);
        assert_eq!(
            outcome,
            Outcome::Error(InferenceError::InvalidUpstream),
            "{input:?}"
        );
        assert!(!got.iter().any(|e| matches!(e, Ev::Finish(_))));
    }
}

/// Every later event is refused once the stream has stopped, and a second
/// start, stop or metadata is refused.
#[test]
fn nothing_is_accepted_after_the_stop_and_nothing_twice() {
    let full = build(&Spec {
        blocks: vec![],
        reason: 0,
        usage: None,
    })
    .events;
    for repeated in 0..full.len() {
        let mut events = full.clone();
        events.insert(repeated, full[repeated].clone());
        let (_, outcome, _) = run(&events, None);
        assert_eq!(
            outcome,
            Outcome::Error(InferenceError::InvalidUpstream),
            "{:?}",
            events[repeated]
        );
    }
    let mut events = full;
    events.insert(
        2,
        W::Text {
            index: 0,
            text: "late".into(),
        },
    );
    let (_, outcome, _) = run(&events, None);
    assert_eq!(outcome, Outcome::Error(InferenceError::InvalidUpstream));
}
