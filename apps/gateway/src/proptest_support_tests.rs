//! Shared generators for the property tests of untrusted-input parsers and
//! encoders (#593): hostile JSON values, random JSON, and mutations of a
//! valid event sequence (drop, duplicate, swap, truncate, wrong types, unknown
//! fields). Test-only; nothing here is production code.
use crate::inference::{
    error::InferenceError,
    types::{ChatEvent, FinishReason, Usage},
};
use proptest::prelude::*;
use serde_json::{Map, Value, json};

/// Explicit case count, no regression file written next to the source (a failing
/// case is reported with its shrunk input in the test output instead), and a
/// bounded shrink so a failure cannot run away.
pub(crate) fn config(cases: u32) -> ProptestConfig {
    ProptestConfig {
        cases,
        failure_persistence: None,
        max_shrink_iters: 4096,
        ..ProptestConfig::default()
    }
}

/// Every value `hostile()` draws from, as a list: for exhaustive sweeps.
pub(crate) fn hostile_values() -> Vec<Value> {
    let mut v = vec![Value::Null, json!(true), json!(false)];
    v.extend([
        json!(0),
        json!(1),
        json!(2),
        json!(3),
        json!(127),
        json!(128),
        json!(255),
        json!(256),
        json!(257),
        json!(-1),
        json!(i64::MIN),
        json!(i64::MAX),
        json!(i64::MAX as u64 + 1),
        json!(u64::MAX),
        json!(9_007_199_254_740_993_u64),
        json!(i32::MAX),
        json!(i32::MAX as u64 + 1),
    ]);
    v.extend([
        json!(1.5),
        json!(-0.0),
        json!(1e308),
        json!(-1e308),
        json!(1e-320),
        json!(0.000001),
    ]);
    v.extend([
        json!(""),
        json!(" "),
        json!("\u{0}"),
        json!("日本語😀"),
        Value::String("a".repeat(5000)),
        Value::String("9".repeat(70)),
        json!("NaN"),
        json!("Infinity"),
        json!("-1"),
        json!("1e999"),
        json!("0x10"),
        json!("1_000"),
    ]);
    v.extend([
        json!([]),
        json!({}),
        json!([1, "a", null]),
        json!({"a": {"b": []}}),
        json!([[]]),
    ]);
    v
}

/// A value of the wrong or extreme kind for whatever field it replaces: nulls,
/// booleans, negative and boundary integers, huge numbers, floats, empty and
/// oversized strings, number-looking strings, empty and nested containers.
pub(crate) fn hostile() -> impl Strategy<Value = Value> {
    prop_oneof![
        9 => prop::sample::select(hostile_values()),
        1 => "[ -~]{1,12}".prop_map(Value::from),
    ]
}

/// A small, plausible value for a field a parser reads (indices, ids, short
/// text), mixed with hostile and random ones: random events that sometimes
/// get past the first field check.
pub(crate) fn field_value() -> impl Strategy<Value = Value> {
    prop_oneof![
        4 => (0..4_u64).prop_map(Value::from),
        3 => prop::sample::select(vec!["m", "x", "msg_0", "fc_0", "call_0", "end_turn", "completed", "assistant", "message", "output_text", "text", "function_call"]).prop_map(Value::from),
        2 => hostile(),
        2 => arb_json(),
    ]
}

/// A random event with a real `type` and fields named as the parser reads them.
pub(crate) fn fuzz_event(
    kinds: &'static [&'static str],
    names: &'static [&'static str],
) -> impl Strategy<Value = Value> {
    (
        prop::sample::select(kinds.to_vec()),
        prop::collection::vec((prop::sample::select(names.to_vec()), field_value()), 0..6),
    )
        .prop_map(|(kind, fields)| {
            let mut m = Map::new();
            m.insert("type".into(), Value::String(kind.into()));
            for (k, v) in fields {
                m.insert(k.into(), v);
            }
            Value::Object(m)
        })
}

/// Fully random JSON of bounded depth and width.
pub(crate) fn arb_json() -> impl Strategy<Value = Value> {
    let key = prop_oneof![
        Just("type".to_owned()),
        Just("index".to_owned()),
        Just("delta".to_owned()),
        Just("usage".to_owned()),
        "[a-z_]{1,8}",
    ];
    let leaf = prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::from),
        any::<i64>().prop_map(Value::from),
        any::<u64>().prop_map(Value::from),
        (-1.0e12..1.0e12_f64).prop_map(|f| json!(f)),
        "[ -~]{0,12}".prop_map(Value::from),
        hostile(),
    ];
    leaf.prop_recursive(3, 24, 4, move |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..4).prop_map(Value::Array),
            prop::collection::vec((key.clone(), inner), 0..4)
                .prop_map(|kv| Value::Object(kv.into_iter().collect::<Map<_, _>>())),
        ]
    })
}

/// One change to an event sequence.
#[derive(Clone, Debug)]
pub(crate) enum Mutation {
    Drop(usize),
    Duplicate(usize),
    Swap(usize, usize),
    Truncate(usize),
    /// Replace one node of event `at` (chosen by `pick`) with `value`.
    Set {
        at: usize,
        pick: usize,
        value: Value,
    },
    /// Remove one node of event `at`.
    Remove {
        at: usize,
        pick: usize,
    },
    /// Add an unknown, non-empty field to a random object node of event `at`,
    /// at any depth (the nodes the parsers guard with `fields(..)`).
    AddNested {
        at: usize,
        pick: usize,
        key: String,
        value: Value,
    },
    /// Add an unknown field to the top level of event `at`.
    AddField {
        at: usize,
        key: String,
        value: Value,
    },
    /// Change the event's `type` to another name (known or unknown).
    Retype {
        at: usize,
        kind: String,
    },
}

/// A mutation drawing event names for `Retype` from `kinds` plus junk names.
pub(crate) fn mutation(kinds: &'static [&'static str]) -> impl Strategy<Value = Mutation> {
    let kind = prop_oneof![
        prop::sample::select(kinds.to_vec()).prop_map(str::to_owned),
        Just(String::new()),
        Just("totally_unknown_event".to_owned()),
        "[a-z._]{1,16}",
    ];
    prop_oneof![
        any::<usize>().prop_map(Mutation::Drop),
        any::<usize>().prop_map(Mutation::Duplicate),
        (any::<usize>(), any::<usize>()).prop_map(|(a, b)| Mutation::Swap(a, b)),
        any::<usize>().prop_map(Mutation::Truncate),
        (any::<usize>(), any::<usize>(), hostile()).prop_map(|(at, pick, value)| Mutation::Set {
            at,
            pick,
            value
        }),
        (any::<usize>(), any::<usize>()).prop_map(|(at, pick)| Mutation::Remove { at, pick }),
        (any::<usize>(), "[a-z_]{1,10}", hostile())
            .prop_map(|(at, key, value)| Mutation::AddField { at, key, value }),
        (
            any::<usize>(),
            any::<usize>(),
            "[a-z_]{1,10}",
            prop::sample::select(vec![
                json!("x"),
                json!(1),
                json!(true),
                json!([1]),
                json!({"a": 1})
            ])
        )
            .prop_map(|(at, pick, key, value)| Mutation::AddNested {
                at,
                pick,
                key,
                value
            }),
        (any::<usize>(), kind).prop_map(|(at, kind)| Mutation::Retype { at, kind }),
    ]
}

pub(crate) type Path = Vec<Seg>;
#[derive(Clone, Debug)]
pub(crate) enum Seg {
    Key(String),
    Item(usize),
}

pub(crate) fn paths(value: &Value, here: &mut Path, out: &mut Vec<Path>) {
    match value {
        Value::Object(map) => {
            for (k, v) in map {
                here.push(Seg::Key(k.clone()));
                out.push(here.clone());
                paths(v, here, out);
                here.pop();
            }
        }
        Value::Array(list) => {
            for (i, v) in list.iter().enumerate() {
                here.push(Seg::Item(i));
                out.push(here.clone());
                paths(v, here, out);
                here.pop();
            }
        }
        _ => {}
    }
}

pub(crate) fn parent_mut<'a>(root: &'a mut Value, path: &[Seg]) -> Option<&'a mut Value> {
    let mut node = root;
    for seg in path {
        node = match seg {
            Seg::Key(k) => node.get_mut(k.as_str())?,
            Seg::Item(i) => node.get_mut(*i)?,
        };
    }
    Some(node)
}

pub(crate) fn pick_path(event: &Value, pick: usize) -> Option<Path> {
    let mut all = vec![];
    paths(event, &mut vec![], &mut all);
    (!all.is_empty()).then(|| all.swap_remove(pick % all.len()))
}

/// The path of every object node of `value`, the root (empty path) included.
pub(crate) fn object_paths(value: &Value) -> Vec<Path> {
    fn walk(value: &Value, here: &mut Path, out: &mut Vec<Path>) {
        match value {
            Value::Object(map) => {
                out.push(here.clone());
                for (k, v) in map {
                    here.push(Seg::Key(k.clone()));
                    walk(v, here, out);
                    here.pop();
                }
            }
            Value::Array(list) => {
                for (i, v) in list.iter().enumerate() {
                    here.push(Seg::Item(i));
                    walk(v, here, out);
                    here.pop();
                }
            }
            _ => {}
        }
    }
    let mut out = vec![];
    walk(value, &mut vec![], &mut out);
    out
}

/// `a.b[2].c` for a path, for failure messages.
pub(crate) fn path_label(path: &[Seg]) -> String {
    let mut out = String::from("$");
    for seg in path {
        match seg {
            Seg::Key(k) => out.push_str(&format!(".{k}")),
            Seg::Item(i) => out.push_str(&format!("[{i}]")),
        }
    }
    out
}

/// Replace the node at `path` (the root when empty).
pub(crate) fn set_path(root: &mut Value, path: &[Seg], value: Value) {
    match path.split_last() {
        None => *root = value,
        Some((last, prefix)) => {
            if let Some(parent) = parent_mut(root, prefix) {
                match last {
                    Seg::Key(k) => parent[k.as_str()] = value,
                    Seg::Item(i) => parent[*i] = value,
                }
            }
        }
    }
}

/// Apply one mutation in place (indices wrap, so every mutation applies to
/// every non-empty sequence).
pub(crate) fn apply(events: &mut Vec<Value>, m: &Mutation) {
    if events.is_empty() {
        return;
    }
    let n = events.len();
    match m {
        Mutation::Drop(i) => {
            events.remove(i % n);
        }
        Mutation::Duplicate(i) => {
            let e = events[i % n].clone();
            events.insert(i % n, e);
        }
        Mutation::Swap(a, b) => events.swap(a % n, b % n),
        Mutation::Truncate(len) => events.truncate(len % (n + 1)),
        Mutation::Set { at, pick, value } => {
            let event = &mut events[at % n];
            if let Some(path) = pick_path(event, *pick) {
                let (last, prefix) = path.split_last().unwrap();
                if let Some(parent) = parent_mut(event, prefix) {
                    match last {
                        Seg::Key(k) => {
                            parent[k.as_str()] = value.clone();
                        }
                        Seg::Item(i) => {
                            parent[*i] = value.clone();
                        }
                    }
                }
            } else {
                events[at % n] = value.clone();
            }
        }
        Mutation::Remove { at, pick } => {
            let event = &mut events[at % n];
            if let Some(path) = pick_path(event, *pick) {
                let (last, prefix) = path.split_last().unwrap();
                if let Some(parent) = parent_mut(event, prefix) {
                    match last {
                        Seg::Key(k) => {
                            parent.as_object_mut().unwrap().remove(k);
                        }
                        Seg::Item(i) => {
                            parent.as_array_mut().unwrap().remove(*i);
                        }
                    }
                }
            }
        }
        Mutation::AddNested {
            at,
            pick,
            key,
            value,
        } => {
            let event = &mut events[at % n];
            let mut nodes = object_paths(event);
            if nodes.is_empty() {
                return;
            }
            let path = nodes.swap_remove(pick % nodes.len());
            if let Some(map) = parent_mut(event, &path).and_then(Value::as_object_mut) {
                map.insert(key.clone(), value.clone());
            }
        }
        Mutation::AddField { at, key, value } => {
            if let Some(map) = events[at % n].as_object_mut() {
                map.insert(key.clone(), value.clone());
            }
        }
        Mutation::Retype { at, kind } => {
            if let Some(map) = events[at % n].as_object_mut() {
                map.insert("type".into(), Value::String(kind.clone()));
            }
        }
    }
}

/// Apply a list of mutations in order.
pub(crate) fn apply_all(events: &mut Vec<Value>, mutations: &[Mutation]) {
    for m in mutations {
        apply(events, m);
    }
}

/// Split `text` into `parts` pieces at char boundaries chosen by `cuts`
/// (empty pieces allowed); the pieces always concatenate back to `text`.
pub(crate) fn split_text(text: &str, cuts: &[usize]) -> Vec<String> {
    let boundaries: Vec<usize> = text
        .char_indices()
        .map(|(i, _)| i)
        .chain([text.len()])
        .collect();
    let mut points: Vec<usize> = cuts
        .iter()
        .map(|c| boundaries[c % boundaries.len()])
        .collect();
    points.sort_unstable();
    let mut out = vec![];
    let mut from = 0;
    for p in points {
        out.push(text[from..p].to_owned());
        from = p;
    }
    out.push(text[from..].to_owned());
    out
}

/// A stream event in comparable, printable form (`ChatEvent` is neither).
// `Usage` is a copyable, fixed-size record, as in `ChatEvent`.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Ev {
    Text(String),
    ToolStart {
        index: u32,
        id: String,
        name: String,
    },
    ToolArgs {
        index: u32,
        args: String,
    },
    Block(Value),
    Finish(FinishReason),
    Usage(Usage),
    Done,
}

pub(crate) fn flatten(event: ChatEvent) -> Vec<Ev> {
    match event {
        ChatEvent::MessagesBlock(v) => vec![Ev::Block(v)],
        ChatEvent::Delta { text, tool_calls } => {
            let mut out = vec![];
            if let Some(t) = text {
                out.push(Ev::Text(t));
            }
            for c in tool_calls {
                if let Some(id) = c.id {
                    out.push(Ev::ToolStart {
                        index: c.index,
                        id,
                        name: c.name.unwrap_or_default(),
                    });
                }
                if let Some(args) = c.arguments {
                    out.push(Ev::ToolArgs {
                        index: c.index,
                        args,
                    });
                }
            }
            out
        }
        ChatEvent::Finish(r) => vec![Ev::Finish(r)],
        ChatEvent::Usage(u) => vec![Ev::Usage(u)],
        ChatEvent::Done => vec![Ev::Done],
    }
}

/// How a driven stream ended.
#[derive(Debug, PartialEq)]
pub(crate) enum Outcome {
    /// The parser emitted `Done` and the driver stopped reading.
    Done,
    /// A frame was refused; the stream ends with this error.
    Error(InferenceError),
    /// The frames ran out before a `Done`: `framing::frames` turns this into
    /// `InvalidUpstream` (its final `Err(InvalidUpstream)?`), never a success.
    Eof,
}

/// Feed `events` to a parser exactly as the adapters' `execute` loops do: yield
/// every event a push returns, return at the first `Done`, end the stream at
/// the first error, and treat running out of frames as `Eof`. Also returns how
/// many frames were consumed (the last one is the frame that ended the stream).
pub(crate) fn drive(
    mut push: impl FnMut(Value) -> Result<Vec<ChatEvent>, InferenceError>,
    events: &[Value],
) -> (Vec<Ev>, Outcome, usize) {
    let mut out = vec![];
    for (i, event) in events.iter().enumerate() {
        match push(event.clone()) {
            Err(e) => return (out, Outcome::Error(e), i + 1),
            Ok(list) => {
                for e in list {
                    let done = matches!(e, ChatEvent::Done);
                    out.extend(flatten(e));
                    if done {
                        return (out, Outcome::Done, i + 1);
                    }
                }
            }
        }
    }
    (out, Outcome::Eof, events.len())
}

/// Every string and every unsigned integer anywhere in `value`.
pub(crate) fn leaves(
    value: &Value,
    strings: &mut std::collections::BTreeSet<String>,
    numbers: &mut std::collections::BTreeSet<u64>,
) {
    match value {
        Value::String(s) => {
            strings.insert(s.clone());
        }
        Value::Number(n) => {
            if let Some(n) = n.as_u64() {
                numbers.insert(n);
            }
        }
        Value::Array(a) => a.iter().for_each(|v| leaves(v, strings, numbers)),
        Value::Object(o) => o.values().for_each(|v| leaves(v, strings, numbers)),
        _ => {}
    }
}

/// At most one of each terminal event, in the order finish, usage, done, with
/// nothing after `done`.
pub(crate) fn check_terminals(events: &[Ev]) -> Result<(), TestCaseError> {
    let at = |f: fn(&Ev) -> bool| -> Vec<usize> {
        events
            .iter()
            .enumerate()
            .filter(|(_, e)| f(e))
            .map(|(i, _)| i)
            .collect()
    };
    let finish = at(|e| matches!(e, Ev::Finish(_)));
    let usage = at(|e| matches!(e, Ev::Usage(_)));
    let done = at(|e| matches!(e, Ev::Done));
    prop_assert!(
        finish.len() <= 1 && usage.len() <= 1 && done.len() <= 1,
        "{events:?}"
    );
    if let Some(d) = done.first() {
        prop_assert_eq!(*d, events.len() - 1, "events after Done");
        prop_assert!(
            finish.len() == 1 && usage.len() == 1,
            "Done without Finish and Usage"
        );
    }
    if let (Some(f), Some(u)) = (finish.first(), usage.first()) {
        prop_assert!(f < u, "Usage before Finish");
    }
    Ok(())
}

/// Every single mutation of `events`, one at a time, handed to `check`:
/// each event dropped and duplicated, every pair swapped, every event
/// truncated away from, every node of every event replaced by every
/// `hostile_values()` entry, and every event replaced by each of them. The
/// label names the mutation. Returns how many mutated streams were checked.
pub(crate) fn for_each_single_mutation(
    events: &[Value],
    mut check: impl FnMut(&str, &[Value]) -> Result<(), TestCaseError>,
) -> Result<usize, TestCaseError> {
    let mut count = 0;
    let mut run =
        |label: String, mutated: Vec<Value>, count: &mut usize| -> Result<(), TestCaseError> {
            *count += 1;
            check(&label, &mutated)
        };
    for i in 0..events.len() {
        let mut v = events.to_vec();
        v.remove(i);
        run(format!("drop {i}"), v, &mut count)?;
        let mut v = events.to_vec();
        v.insert(i, events[i].clone());
        run(format!("duplicate {i}"), v, &mut count)?;
        run(format!("truncate to {i}"), events[..i].to_vec(), &mut count)?;
        for j in i + 1..events.len() {
            let mut v = events.to_vec();
            v.swap(i, j);
            run(format!("swap {i} {j}"), v, &mut count)?;
        }
        for value in hostile_values() {
            let mut v = events.to_vec();
            v[i] = value.clone();
            run(format!("event {i} := {value}"), v, &mut count)?;
        }
        let mut all = vec![];
        paths(&events[i], &mut vec![], &mut all);
        for path in &all {
            for value in hostile_values() {
                let mut v = events.to_vec();
                set_path(&mut v[i], path, value.clone());
                let shown: String = value.to_string().chars().take(40).collect();
                run(
                    format!("event {i} {} := {shown}", path_label(path)),
                    v,
                    &mut count,
                )?;
            }
        }
    }
    Ok(count)
}
