//! Property tests of the OpenAI Responses stream parser, `State::push` (#593).
//!
//! The contract these tests state, read from the code and from `execute`'s
//! loop (the only caller):
//!
//! * `push` never panics. Its errors are the documented classes only: a frame
//!   that is not an upstream failure event is `InvalidUpstream`; an `error` or
//!   `response.failed` event is classified like the same error as an HTTP
//!   response (`stream_error::classify` over `framing::status_error`:
//!   `Busy`, `Timeout`, `Configuration`, `UpstreamRejected`,
//!   `UpstreamUnavailable`, or `InvalidUpstream` for a numeric code that is no
//!   error status). The class never depends on the message text, which is
//!   never reflected.
//! * Termination. A stream ends only at `response.completed` /
//!   `response.incomplete`, and then `push` returns exactly `Finish`, `Usage`,
//!   `Done` in that order. `execute` returns at the first `Done`, so a driver
//!   sees at most one terminal and nothing after it. Frames running out before
//!   a terminal is `framing::frames`' final `Err(TRANSPORT)` (a transport
//!   failure, `UpstreamUnavailable`), never a success; the parser fabricates nothing for a truncated stream.
//!   After emitting `Done`, `State` refuses every frame with `InvalidUpstream`.
//! * Reconciliation. `Done` is emitted only when the terminal snapshot equals
//!   what was streamed: the text deltas concatenate to the snapshot's message
//!   text, and the streamed tool calls (ids, names, argument fragments) equal
//!   the snapshot's function calls, in order.
//! * Usage is exactly what the terminal snapshot reported (absent stays unknown,
//!   never `0`; cache read and write round-trip), and a snapshot that fails
//!   reconciliation still reports its valid usage as evidence. Only the
//!   directly reported fields (input, output, cache read, cache write,
//!   reasoning) are compared, plus `total >= input`; the derived fields
//!   (uncached input, the write allocations) are `providers/metering.rs`'s
//!   rules (`docs/cache-pricing.md`) and belong to its tests.
//! * Unknown fields. Every object the parser guards with `fields(..)` (an
//!   output item, a content part, a terminal snapshot's output items and their
//!   parts) refuses an unknown non-empty field at any depth; the table of
//!   nodes that tolerate extras is `tolerates_unknown_field`.
//! * A failing generated case must be pasted into a named regression test:
//!   failure seeds are not persisted (`failure_persistence: None`).
//! * Limits refuse rather than truncate: 129 output items, 128 tool calls and
//!   4 MiB of streamed bytes.
use super::*;
use crate::{
    billing::BillingUsage,
    proptest_support_tests::{
        Ev, Outcome, Seg, apply_all, arb_json, check_terminals, config, drive,
        for_each_single_mutation, fuzz_event, leaves, mutation, object_paths, path_label,
        split_text,
    },
};
use proptest::prelude::*;
use serde_json::Map;
use std::collections::BTreeSet;

// ---- spec ------------------------------------------------------------------

#[derive(Clone, Debug)]
enum Item {
    Message {
        phase: Option<bool>,
        pieces: Vec<String>,
    },
    Call {
        name: String,
        args: Value,
        cuts: Vec<usize>,
        empty: bool,
    },
    Reasoning {
        encrypted: Option<String>,
    },
}

#[derive(Clone, Copy, Debug)]
enum End {
    Completed,
    Length,
    Filter,
}

#[derive(Clone, Debug)]
struct Counts {
    input: u64,
    cached: Option<u64>,
    write: Option<u64>,
    output: u64,
    reasoning: Option<u64>,
}

#[derive(Clone, Debug)]
struct Spec {
    items: Vec<Item>,
    end: End,
    usage: Option<Counts>,
    in_progress: bool,
}

fn piece() -> impl Strategy<Value = String> {
    prop_oneof![
        3 => "[ -~\n]{0,12}",
        1 => prop::collection::vec(any::<char>(), 0..6).prop_map(|v| v.into_iter().collect::<String>()),
    ]
}

fn item() -> impl Strategy<Value = Item> {
    prop_oneof![
        3 => (prop::option::of(any::<bool>()), prop::collection::vec(piece(), 0..4))
            .prop_map(|(phase, pieces)| Item::Message { phase, pieces }),
        3 => ("[A-Za-z0-9_-]{1,12}", arb_json(), prop::collection::vec(any::<usize>(), 0..4), 0..6_u8)
            .prop_map(|(name, args, cuts, empty)| Item::Call {
                name,
                args: if args.is_object() { args } else { json!({ "value": args }) },
                cuts,
                empty: empty == 0,
            }),
        1 => prop::option::of("[a-z0-9]{0,8}").prop_map(|encrypted| Item::Reasoning { encrypted }),
    ]
}

fn counts() -> impl Strategy<Value = Counts> {
    let size = prop_oneof![3 => 0..2000_u64, 1 => 0..=(i64::MAX as u64 / 4)];
    (
        size.clone(),
        prop::option::of(any::<u64>()),
        prop::option::of(any::<u64>()),
        size,
        prop::option::of(any::<u64>()),
    )
        .prop_map(|(input, cached, write, output, reasoning)| {
            let cached = cached.map(|c| c % (input + 1));
            let write = write.map(|w| w % (input - cached.unwrap_or(0) + 1));
            Counts {
                input,
                cached,
                write,
                output,
                reasoning: reasoning.map(|r| r % (output + 1)),
            }
        })
}

fn spec() -> impl Strategy<Value = Spec> {
    (
        prop::collection::vec(item(), 0..6),
        prop::sample::select(vec![
            End::Completed,
            End::Completed,
            End::Length,
            End::Filter,
        ]),
        prop::option::of(counts()),
        any::<bool>(),
    )
        .prop_map(|(items, end, usage, in_progress)| Spec {
            items,
            end,
            usage,
            in_progress,
        })
}

// ---- building the wire stream and what it must produce -----------------------

fn usage_json(c: &Counts) -> Value {
    let mut u = json!({"input_tokens": c.input, "output_tokens": c.output});
    let mut details = Map::new();
    if let Some(r) = c.cached {
        details.insert("cached_tokens".into(), r.into());
    }
    if let Some(w) = c.write {
        details.insert("cache_write_tokens".into(), w.into());
    }
    if !details.is_empty() {
        u["input_tokens_details"] = Value::Object(details);
    }
    if let Some(r) = c.reasoning {
        u["output_tokens_details"] = json!({"reasoning_tokens": r});
    }
    u
}

/// The directly reported fields only (see the module doc).
fn project(u: &Usage) -> Usage {
    Usage {
        input_tokens: u.input_tokens,
        output_tokens: u.output_tokens,
        reasoning_tokens: u.reasoning_tokens,
        billing: u.billing.map(|b| BillingUsage {
            cache_read_input_tokens: b.cache_read_input_tokens,
            cache_write_input_tokens: b.cache_write_input_tokens,
            ..BillingUsage::default()
        }),
        ..Usage::default()
    }
}

fn expected_usage(c: &Option<Counts>) -> Usage {
    let Some(c) = c else {
        return Usage::default();
    };
    Usage {
        input_tokens: Some(c.input),
        output_tokens: Some(c.output),
        billing: Some(BillingUsage {
            cache_read_input_tokens: c.cached,
            cache_write_input_tokens: c.write,
            ..BillingUsage::default()
        }),
        reasoning_tokens: c.reasoning,
        ..Usage::default()
    }
}

fn message_json(i: usize, phase: Option<bool>, text: &str, done: bool) -> Value {
    let mut m = json!({"id": format!("msg_{i}"), "type": "message", "role": "assistant",
        "status": if done { "completed" } else { "in_progress" },
        "content": if done { json!([{"type": "output_text", "text": text, "annotations": []}]) } else { json!([]) }});
    if let Some(p) = phase {
        m["phase"] = json!(if p { "final_answer" } else { "commentary" });
    }
    m
}

fn call_json(i: usize, name: &str, args: &str, done: bool) -> Value {
    json!({"id": format!("fc_{i}"), "type": "function_call",
        "status": if done { "completed" } else { "in_progress" },
        "call_id": format!("call_{i}"), "name": name, "arguments": if done { args } else { "" }})
}

fn reasoning_json(i: usize, encrypted: &Option<String>, done: bool) -> Value {
    let mut r = json!({"id": format!("rs_{i}"), "type": "reasoning",
        "status": if done { "completed" } else { "in_progress" }, "summary": []});
    if let (true, Some(e)) = (done, encrypted) {
        r["encrypted_content"] = json!(e);
    }
    r
}

struct Built {
    events: Vec<Value>,
    expected: Vec<Ev>,
}

fn build(spec: &Spec) -> Built {
    let mut events =
        vec![json!({"type": "response.created", "response": {"status": "in_progress"}})];
    if spec.in_progress {
        events.push(json!({"type": "response.in_progress", "response": {"status": "in_progress"}}));
    }
    let mut expected = vec![];
    let mut output = vec![];
    let mut calls = 0_u32;
    for (i, item) in spec.items.iter().enumerate() {
        match item {
            Item::Message { phase, pieces } => {
                let id = format!("msg_{i}");
                let full: String = pieces.concat();
                events.push(json!({"type": "response.output_item.added", "output_index": i, "item": message_json(i, *phase, "", false)}));
                events.push(json!({"type": "response.content_part.added", "output_index": i, "content_index": 0, "item_id": id,
                    "part": {"type": "output_text", "text": "", "annotations": []}}));
                for p in pieces {
                    events.push(json!({"type": "response.output_text.delta", "output_index": i, "item_id": id, "content_index": 0, "delta": p}));
                    expected.push(Ev::Text(p.clone()));
                }
                events.push(json!({"type": "response.output_text.done", "output_index": i, "item_id": id, "content_index": 0, "text": full}));
                events.push(json!({"type": "response.content_part.done", "output_index": i, "content_index": 0, "item_id": id,
                    "part": {"type": "output_text", "text": full, "annotations": []}}));
                let done = message_json(i, *phase, &full, true);
                events.push(
                    json!({"type": "response.output_item.done", "output_index": i, "item": done}),
                );
                output.push(done);
            }
            Item::Call {
                name,
                args,
                cuts,
                empty,
            } => {
                let id = format!("fc_{i}");
                let text = if *empty {
                    String::new()
                } else {
                    args.to_string()
                };
                let pieces = if *empty {
                    vec![]
                } else {
                    split_text(&text, cuts)
                };
                events.push(json!({"type": "response.output_item.added", "output_index": i, "item": call_json(i, name, "", false)}));
                expected.push(Ev::ToolStart {
                    index: calls,
                    id: format!("call_{i}"),
                    name: name.clone(),
                });
                for p in &pieces {
                    events.push(json!({"type": "response.function_call_arguments.delta", "output_index": i, "item_id": id, "delta": p}));
                    expected.push(Ev::ToolArgs {
                        index: calls,
                        args: p.clone(),
                    });
                }
                events.push(json!({"type": "response.function_call_arguments.done", "output_index": i, "item_id": id, "arguments": text}));
                let done = call_json(i, name, &text, true);
                events.push(
                    json!({"type": "response.output_item.done", "output_index": i, "item": done}),
                );
                output.push(done);
                calls += 1;
            }
            Item::Reasoning { encrypted } => {
                events.push(json!({"type": "response.output_item.added", "output_index": i, "item": reasoning_json(i, encrypted, false)}));
                let done = reasoning_json(i, encrypted, true);
                events.push(
                    json!({"type": "response.output_item.done", "output_index": i, "item": done}),
                );
                output.push(done);
            }
        }
    }
    let (kind, status, reason, finish) = match spec.end {
        End::Completed => (
            "response.completed",
            "completed",
            None,
            if calls > 0 {
                FinishReason::ToolCalls
            } else {
                FinishReason::Stop
            },
        ),
        End::Length => (
            "response.incomplete",
            "incomplete",
            Some("max_output_tokens"),
            FinishReason::Length,
        ),
        End::Filter => (
            "response.incomplete",
            "incomplete",
            Some("content_filter"),
            FinishReason::ContentFilter,
        ),
    };
    let mut response =
        json!({"id": "resp", "object": "response", "status": status, "output": output});
    if let Some(r) = reason {
        response["incomplete_details"] = json!({ "reason": r });
    }
    if let Some(c) = &spec.usage {
        response["usage"] = usage_json(c);
    }
    events.push(json!({ "type": kind, "response": response }));
    expected.extend([
        Ev::Finish(finish),
        Ev::Usage(expected_usage(&spec.usage)),
        Ev::Done,
    ]);
    Built { events, expected }
}

fn run(events: &[Value]) -> (Vec<Ev>, Outcome, usize) {
    let mut state = State::default();
    let (got, outcome, consumed) = drive(|v| state.push(v), events);
    (normalise(got), outcome, consumed)
}

/// Project every `Usage` onto the directly reported fields.
fn normalise(events: Vec<Ev>) -> Vec<Ev> {
    events
        .into_iter()
        .map(|e| match e {
            Ev::Usage(u) => {
                if let (Some(b), Some(i)) = (u.billing, u.input_tokens) {
                    assert!(
                        b.total_input_tokens.is_none_or(|t| t >= i),
                        "total below input"
                    );
                }
                Ev::Usage(project(&u))
            }
            e => e,
        })
        .collect()
}

// ---- checks on any stream --------------------------------------------------

const FIELDS: &[&str] = &[
    "output_index",
    "item_id",
    "content_index",
    "delta",
    "item",
    "part",
    "response",
    "text",
    "id",
    "status",
    "role",
    "content",
    "phase",
    "arguments",
    "call_id",
    "name",
    "usage",
    "output",
    "annotations",
    "summary",
    "incomplete_details",
    "input_tokens",
    "output_tokens",
];

const KINDS: &[&str] = &[
    "response.created",
    "response.in_progress",
    "response.output_item.added",
    "response.output_item.done",
    "response.content_part.added",
    "response.content_part.done",
    "response.output_text.delta",
    "response.output_text.done",
    "response.function_call_arguments.delta",
    "response.function_call_arguments.done",
    "response.completed",
    "response.incomplete",
    "response.failed",
    "error",
];

fn kind_of(event: &Value) -> &str {
    event["type"].as_str().unwrap_or("")
}

/// Invariants of whatever a driven stream produced, valid or not.
fn check_any(
    events: &[Value],
    got: &[Ev],
    outcome: &Outcome,
    consumed: usize,
) -> std::result::Result<(), TestCaseError> {
    check_terminals(got)?;
    let count = |f: fn(&Ev) -> bool| got.iter().filter(|e| f(e)).count();
    let finish = count(|e| matches!(e, Ev::Finish(_)));
    // The parser emits Finish, Usage and Done together from one terminal frame.
    prop_assert_eq!(finish, count(|e| matches!(e, Ev::Usage(_))));
    prop_assert_eq!(finish, count(|e| matches!(e, Ev::Done)));
    prop_assert!(!got.iter().any(|e| matches!(e, Ev::Block(_))));
    match outcome {
        Outcome::Error(e) => {
            if matches!(kind_of(&events[consumed - 1]), "error" | "response.failed") {
                prop_assert!(
                    matches!(
                        e,
                        InferenceError::Busy
                            | InferenceError::Timeout
                            | InferenceError::Configuration
                            | InferenceError::UpstreamRejected
                            | InferenceError::UpstreamUnavailable
                            | InferenceError::InvalidUpstream
                    ),
                    "{e:?}"
                );
            } else {
                prop_assert_eq!(*e, InferenceError::InvalidUpstream);
            }
        }
        Outcome::Eof => prop_assert_eq!(finish, 0, "a terminal without Done"),
        Outcome::Done => prop_assert_eq!(finish, 1),
    }
    // Everything emitted comes from what the upstream sent.
    let (mut strings, mut numbers) = (BTreeSet::new(), BTreeSet::new());
    for e in &events[..consumed] {
        leaves(e, &mut strings, &mut numbers);
    }
    let mut started = 0_u32;
    for e in got {
        match e {
            Ev::Text(t) => prop_assert!(strings.contains(t), "invented text {t:?}"),
            Ev::ToolStart { index, id, name } => {
                prop_assert_eq!(*index, started, "tool indices are not contiguous");
                prop_assert!(started < 128, "more than 128 tool calls");
                prop_assert!(strings.contains(id) && strings.contains(name));
                started += 1;
            }
            Ev::ToolArgs { index, args } => {
                prop_assert!(*index < started, "arguments for a tool never announced");
                prop_assert!(strings.contains(args), "invented arguments {args:?}");
            }
            _ => {}
        }
    }
    // Done only for a terminal frame, and only if the snapshot equals the stream.
    if *outcome == Outcome::Done {
        let last = &events[consumed - 1];
        let kind = kind_of(last);
        prop_assert!(kind == "response.completed" || kind == "response.incomplete");
        prop_assert!(
            events[..consumed - 1]
                .iter()
                .any(|e| kind_of(e) == "response.created")
        );
        let response = &last["response"];
        // Usage is the terminal snapshot's own, field by field, read here
        // independently: absent or null is unknown, never zero.
        let read = |path: &[&str]| path.iter().fold(&response["usage"], |v, k| &v[*k]).as_u64();
        let output = read(&["output_tokens"]);
        let want = Usage {
            input_tokens: read(&["input_tokens"]),
            output_tokens: output,
            reasoning_tokens: read(&["output_tokens_details", "reasoning_tokens"])
                .filter(|r| *r <= i64::MAX as u64 && output.is_none_or(|o| *r <= o)),
            billing: response["usage"].is_object().then(|| BillingUsage {
                cache_read_input_tokens: read(&["input_tokens_details", "cached_tokens"]),
                cache_write_input_tokens: read(&["input_tokens_details", "cache_write_tokens"]),
                ..BillingUsage::default()
            }),
            ..Usage::default()
        };
        let emitted: Vec<&Usage> = got
            .iter()
            .filter_map(|e| if let Ev::Usage(u) = e { Some(u) } else { None })
            .collect();
        prop_assert_eq!(
            emitted,
            vec![&want],
            "usage differs from the terminal frame's"
        );
        let (mut text, mut calls) = (String::new(), vec![]);
        for item in response["output"].as_array().unwrap() {
            match item["type"].as_str() {
                Some("message") => {
                    for part in item["content"].as_array().unwrap() {
                        text.push_str(part["text"].as_str().unwrap());
                    }
                }
                Some("function_call") => calls.push((
                    item["call_id"].as_str().unwrap().to_owned(),
                    item["name"].as_str().unwrap().to_owned(),
                    item["arguments"].as_str().unwrap().to_owned(),
                )),
                _ => {}
            }
        }
        let streamed: String = got
            .iter()
            .filter_map(|e| {
                if let Ev::Text(t) = e {
                    Some(t.as_str())
                } else {
                    None
                }
            })
            .collect();
        prop_assert_eq!(streamed, text, "streamed text differs from the snapshot");
        let starts: Vec<(String, String)> = got
            .iter()
            .filter_map(|e| {
                if let Ev::ToolStart { id, name, .. } = e {
                    Some((id.clone(), name.clone()))
                } else {
                    None
                }
            })
            .collect();
        prop_assert_eq!(
            starts,
            calls
                .iter()
                .map(|c| (c.0.clone(), c.1.clone()))
                .collect::<Vec<_>>()
        );
        for (i, call) in calls.iter().enumerate() {
            let args: String = got
                .iter()
                .filter_map(|e| match e {
                    Ev::ToolArgs { index, args } if *index as usize == i => Some(args.as_str()),
                    _ => None,
                })
                .collect();
            prop_assert_eq!(
                &args,
                &call.2,
                "streamed arguments differ from the snapshot"
            );
        }
    }
    Ok(())
}

// ---- properties ------------------------------------------------------------

proptest! {
    #![proptest_config(config(1500))]

    /// A valid stream (created, any mix of message, function-call and
    /// reasoning items with their deltas, parts and done events, a terminal
    /// with or without usage) produces exactly the text deltas, tool starts
    /// and argument fragments it carried, in order and with contiguous tool
    /// indices, then one Finish, one Usage equal to the reported counts (absent
    /// stays unknown; cache fields round-trip) and one Done.
    #[test]
    fn a_valid_stream_is_reproduced_exactly_and_terminates_once(spec in spec()) {
        let built = build(&spec);
        let (got, outcome, consumed) = run(&built.events);
        prop_assert_eq!(&outcome, &Outcome::Done);
        prop_assert_eq!(consumed, built.events.len());
        prop_assert_eq!(&got, &built.expected);
        check_any(&built.events, &got, &outcome, consumed)?;
    }

    /// A stream cut anywhere before its terminal never finishes: it ends at
    /// end-of-frames (an error, never a success), emits no Finish, Usage or
    /// Done, and what it did emit is a prefix of the full stream's events.
    #[test]
    fn a_truncated_stream_never_finishes(spec in spec(), cut in any::<usize>()) {
        let built = build(&spec);
        let cut = cut % built.events.len();
        let (got, outcome, _) = run(&built.events[..cut]);
        prop_assert_eq!(&outcome, &Outcome::Eof);
        prop_assert!(!got.iter().any(|e| matches!(e, Ev::Finish(_) | Ev::Usage(_) | Ev::Done)));
        prop_assert_eq!(&got[..], &built.expected[..got.len()]);
    }

    /// A valid stream with events dropped, duplicated, swapped or truncated,
    /// fields changed to the wrong type, huge or negative numbers, empty and
    /// oversized strings, unknown fields and unknown event types: never a
    /// panic, only `InvalidUpstream`, at most one terminal and nothing after
    /// it, Done only from a terminal frame whose snapshot equals the stream,
    /// and no emitted text, tool call or count the upstream did not send.
    #[test]
    fn a_mutated_stream_is_refused_or_reconciled(
        spec in spec(),
        mutations in prop::collection::vec(mutation(KINDS), 1..6),
    ) {
        let mut events = build(&spec).events;
        apply_all(&mut events, &mutations);
        let (got, outcome, consumed) = run(&events);
        check_any(&events, &got, &outcome, consumed)?;
    }

    /// Random events with a real `type` and the fields the parser reads, after
    /// a plausible start: no panic, only `InvalidUpstream`.
    #[test]
    fn arbitrary_events_are_refused_without_panic(
        events in prop::collection::vec(fuzz_event(KINDS, FIELDS), 0..14),
        started in any::<bool>(),
    ) {
        let mut all = vec![];
        if started {
            all.push(json!({"type": "response.created", "response": {"status": "in_progress"}}));
        }
        all.extend(events);
        let (got, outcome, consumed) = run(&all);
        check_any(&all, &got, &outcome, consumed)?;
    }

    /// Arbitrary JSON events (mostly refused at the first type check).
    #[test]
    fn arbitrary_json_is_refused_without_panic(events in prop::collection::vec(arb_json(), 0..8)) {
        let (got, outcome, consumed) = run(&events);
        check_any(&events, &got, &outcome, consumed)?;
    }
}

proptest! {
    #![proptest_config(config(300))]

    /// A terminal snapshot that disagrees with the stream fails the attempt but
    /// its valid usage is kept as evidence, exactly as reported (and nothing is
    /// recorded when the snapshot reported no usage).
    #[test]
    fn a_rejected_terminal_still_reports_only_its_own_usage(
        spec in spec(),
        first in piece(),
    ) {
        let mut spec = spec;
        spec.items.insert(0, Item::Message { phase: None, pieces: vec![first] });
        let mut events = build(&spec).events;
        let last = events.last_mut().unwrap();
        let text = last["response"]["output"][0]["content"][0]["text"].as_str().unwrap().to_owned();
        last["response"]["output"][0]["content"][0]["text"] = json!(format!("{text}!"));
        let runtime = tokio::runtime::Builder::new_current_thread().build().unwrap();
        let (result, evidence) = runtime.block_on(crate::inference::evidence::capture(async {
            let mut state = State::default();
            for e in &events {
                state.push(e.clone())?;
            }
            Ok::<(), InferenceError>(())
        }));
        prop_assert_eq!(result, Err(InferenceError::InvalidUpstream));
        let want = expected_usage(&spec.usage);
        match evidence {
            None => prop_assert_eq!(want, Usage::default()),
            Some(u) => prop_assert_eq!(project(&u), want),
        }
    }
}

// ---- limits and named cases ------------------------------------------------

fn reasoning_stream(items: usize) -> Vec<Value> {
    build(&Spec {
        items: (0..items)
            .map(|_| Item::Reasoning { encrypted: None })
            .collect(),
        end: End::Completed,
        usage: None,
        in_progress: false,
    })
    .events
}

fn call_stream(calls: usize) -> Vec<Value> {
    build(&Spec {
        items: (0..calls)
            .map(|i| Item::Call {
                name: format!("f{i}"),
                args: json!({}),
                cuts: vec![],
                empty: false,
            })
            .collect(),
        end: End::Completed,
        usage: None,
        in_progress: false,
    })
    .events
}

/// 129 output items and 128 tool calls are accepted whole; one more of either
/// is refused (at the item that would exceed it), never truncated.
#[test]
fn item_and_tool_limits_refuse_the_next_unit() {
    let (got, outcome, _) = run(&reasoning_stream(129));
    assert_eq!(outcome, Outcome::Done, "129 items");
    assert!(matches!(got.last(), Some(Ev::Done)));
    let (got, outcome, _) = run(&reasoning_stream(130));
    assert_eq!(
        outcome,
        Outcome::Error(InferenceError::InvalidUpstream),
        "130 items"
    );
    assert!(!got.iter().any(|e| matches!(e, Ev::Done | Ev::Finish(_))));

    let (got, outcome, _) = run(&call_stream(128));
    assert_eq!(outcome, Outcome::Done, "128 calls");
    assert_eq!(
        got.iter()
            .filter(|e| matches!(e, Ev::ToolStart { .. }))
            .count(),
        128
    );
    let (got, outcome, _) = run(&call_stream(129));
    assert_eq!(
        outcome,
        Outcome::Error(InferenceError::InvalidUpstream),
        "129 calls"
    );
    assert_eq!(
        got.iter()
            .filter(|e| matches!(e, Ev::ToolStart { .. }))
            .count(),
        128,
        "the 129th is not announced"
    );
}

/// More than 4 MiB of streamed text is refused at the delta that crosses it;
/// everything before it was delivered whole, and nothing is cut short.
#[test]
fn streamed_bytes_are_bounded_and_refused_not_truncated() {
    let chunk = "x".repeat(1024 * 1024);
    let spec = Spec {
        items: vec![Item::Message {
            phase: None,
            pieces: vec![chunk; 5],
        }],
        end: End::Completed,
        usage: None,
        in_progress: false,
    };
    let built = build(&spec);
    let (got, outcome, _) = run(&built.events);
    assert_eq!(outcome, Outcome::Error(InferenceError::InvalidUpstream));
    let delivered: usize = got
        .iter()
        .map(|e| if let Ev::Text(t) = e { t.len() } else { 0 })
        .sum();
    assert!(
        (3 * 1024 * 1024..=4 * 1024 * 1024).contains(&delivered),
        "{delivered}"
    );
    assert!(!got.iter().any(|e| matches!(e, Ev::Done | Ev::Finish(_))));
}

/// The upstream's failure events are classified like the same error as an
/// HTTP response, by the documented code alone; the message text is never
/// consulted or reflected. `response.cancelled` and `response.queued` are not
/// failures this adapter accepts.
#[test]
fn upstream_failure_events_are_classified_by_their_code() {
    use InferenceError as E;
    let created = json!({"type": "response.created", "response": {"status": "in_progress"}});
    for (code, class) in [
        ("server_error", E::UpstreamUnavailable),
        ("rate_limit_exceeded", E::Busy),
        ("insufficient_quota", E::Busy),
        ("invalid_prompt", E::UpstreamRejected),
        ("invalid_image_url", E::UpstreamRejected),
        ("invalid_api_key", E::Configuration),
        ("model_not_found", E::UpstreamRejected),
        // Not a documented name: unavailable, never a client fault.
        ("vector_store_timeout", E::UpstreamUnavailable),
        ("something_new", E::UpstreamUnavailable),
    ] {
        for started in [true, false] {
            let stream = if started {
                vec![created.clone()]
            } else {
                vec![]
            };
            let failed = json!({"type": "response.failed", "response": {"status": "failed", "error": {"code": code, "message": "rate_limit_exceeded secret text"}}});
            let error = json!({"type": "error", "code": code, "message": "invalid_prompt secret text", "param": null});
            for event in [failed, error] {
                let mut events = stream.clone();
                events.push(event.clone());
                let (got, outcome, _) = run(&events);
                assert_eq!(outcome, Outcome::Error(class), "{event}");
                assert!(!got.iter().any(|e| matches!(e, Ev::Done | Ev::Finish(_))));
            }
        }
    }
    // A failure with no readable code is an unavailable provider, not a success.
    for event in [
        json!({"type": "response.failed", "response": {"status": "failed", "error": null}}),
        json!({"type": "response.failed", "response": {"status": "failed"}}),
        json!({"type": "response.failed"}),
        json!({"type": "error", "message": "secret text"}),
        json!({"type": "error", "code": {"nested": true}}),
    ] {
        let (_, outcome, _) = run(std::slice::from_ref(&event));
        assert_eq!(outcome, Outcome::Error(E::UpstreamUnavailable), "{event}");
    }
    for kind in ["response.cancelled", "response.queued"] {
        let (_, outcome, _) = run(&[
            created.clone(),
            json!({"type": kind, "response": {"status": "failed", "error": {"code": "server_error"}}}),
        ]);
        assert_eq!(outcome, Outcome::Error(E::InvalidUpstream), "{kind}");
    }
}

#[test]
fn a_terminal_pushed_after_the_terminal_is_refused() {
    for end in [End::Completed, End::Length, End::Filter] {
        let built = build(&Spec {
            items: vec![],
            end,
            usage: None,
            in_progress: false,
        });
        let mut state = State::default();
        for event in &built.events {
            state.push(event.clone()).unwrap();
        }
        for late in [
            built.events.last().unwrap().clone(),
            json!({"type":"response.in_progress"}),
            json!({"type":"error","code":"rate_limit_exceeded"}),
        ] {
            assert!(matches!(
                state.push(late),
                Err(InferenceError::InvalidUpstream)
            ));
        }
    }
}

// ---- deterministic sweeps ----------------------------------------------------

/// A canonical stream with every item kind, a phase, tool arguments in
/// fragments, reasoning, `in_progress`, usage with cache and reasoning counts.
fn rich(end: End) -> Built {
    build(&Spec {
        items: vec![
            Item::Message {
                phase: Some(true),
                pieces: vec!["he".into(), "llo".into()],
            },
            Item::Call {
                name: "get".into(),
                args: json!({"a": 1, "b": [1, 2]}),
                cuts: vec![3, 5],
                empty: false,
            },
            Item::Reasoning {
                encrypted: Some("e".into()),
            },
            Item::Message {
                phase: None,
                pieces: vec!["!".into()],
            },
        ],
        end,
        usage: Some(Counts {
            input: 100,
            cached: Some(10),
            write: Some(5),
            output: 20,
            reasoning: Some(7),
        }),
        in_progress: true,
    })
}

/// The nodes that tolerate an unknown field, read from the code. Everything
/// else is guarded by `fields(..)` and refuses one.
///
/// * every event's top level: only named fields are read;
/// * `response` of `response.created` / `response.in_progress`: never read;
/// * a terminal's `response` and everything under it except `output`: only
///   the named keys of `usage` and the `status`/`incomplete_details.reason`
///   are read (`decode_shape` does not call `fields` on the response);
/// * refused: `item` and `part` (`message_fields`, `reasoning_fields`, the
///   function-call and content-part `fields`), their content parts, and the
///   items and parts under a terminal's `response.output`.
fn tolerates_unknown_field(event: &Value, path: &[Seg]) -> bool {
    let Some(Seg::Key(first)) = path.first() else {
        return true;
    };
    match (kind_of(event), first.as_str()) {
        ("response.created" | "response.in_progress", _) => true,
        ("response.completed" | "response.incomplete", "response") => {
            !matches!(path.get(1), Some(Seg::Key(k)) if k == "output")
        }
        (_, "item" | "part") => false,
        _ => true,
    }
}

/// An unknown, non-empty `zz_unknown` field injected into every object node of
/// every event of a rich valid stream is refused (`InvalidUpstream`) except at
/// the pinned table of tolerant nodes, where the stream is unchanged.
#[test]
fn an_unknown_nested_field_is_refused_except_where_the_table_allows_it() {
    let (mut refused, mut tolerated) = (0, 0);
    for end in [End::Completed, End::Length] {
        let built = rich(end);
        for (i, event) in built.events.iter().enumerate() {
            for path in object_paths(event) {
                let mut events = built.events.clone();
                let node = path.iter().fold(&mut events[i], |v, seg| match seg {
                    Seg::Key(k) => &mut v[k.as_str()],
                    Seg::Item(n) => &mut v[*n],
                });
                node["zz_unknown"] = json!("x");
                let (got, outcome, _) = run(&events);
                let label = format!("{} in event {i} ({})", path_label(&path), kind_of(event));
                if tolerates_unknown_field(event, &path) {
                    assert_eq!(outcome, Outcome::Done, "{label} should be tolerated");
                    assert_eq!(got, built.expected, "{label}");
                    tolerated += 1;
                } else {
                    assert_eq!(
                        outcome,
                        Outcome::Error(InferenceError::InvalidUpstream),
                        "{label} should be refused"
                    );
                    refused += 1;
                }
            }
        }
    }
    assert!(
        refused > 30 && tolerated > 30,
        "refused {refused}, tolerated {tolerated}"
    );
}

/// Every single mutation of a rich valid stream, one at a time: each event
/// dropped, duplicated and truncated at, every pair swapped, every event and
/// every node of every event replaced by every hostile value. All pass through
/// `check_any`, and enough of them still reach `Done` for the snapshot
/// reconciliation and the independent usage read to run.
#[test]
fn every_single_mutation_of_a_rich_stream_is_refused_or_reconciled() {
    let built = rich(End::Completed);
    let mut done = 0;
    let total = for_each_single_mutation(&built.events, |label, events| {
        let (got, outcome, consumed) = run(events);
        if outcome == Outcome::Done {
            done += 1;
        }
        check_any(events, &got, &outcome, consumed)
            .map_err(|e| TestCaseError::fail(format!("{label}: {e}")))
    })
    .unwrap();
    println!("openai sweep: {total} single mutations, {done} still reach Done");
    assert!(
        total > 5000 && done > 100,
        "{total} mutations, {done} reach Done"
    );
}
