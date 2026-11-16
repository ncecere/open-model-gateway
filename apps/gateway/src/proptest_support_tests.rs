//! Shared generators for the property tests of untrusted-input parsers and
//! encoders (#593): hostile JSON values, random JSON, and mutations of a
//! valid event sequence (drop, duplicate, swap, truncate, wrong types, unknown
//! fields). Test-only; nothing here is production code.
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

/// A value of the wrong or extreme kind for whatever field it replaces: nulls,
/// booleans, negative and boundary integers, huge numbers, floats, empty and
/// oversized strings, number-looking strings, empty and nested containers.
pub(crate) fn hostile() -> impl Strategy<Value = Value> {
    let ints = vec![
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
    ];
    let floats = vec![
        json!(1.5),
        json!(-0.0),
        json!(1e308),
        json!(-1e308),
        json!(1e-320),
        json!(0.000001),
    ];
    let strings = vec![
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
    ];
    let containers = vec![
        json!([]),
        json!({}),
        json!([1, "a", null]),
        json!({"a": {"b": []}}),
        json!([[]]),
    ];
    prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::from),
        prop::sample::select(ints),
        prop::sample::select(floats),
        prop::sample::select(strings),
        "[ -~]{1,12}".prop_map(Value::from),
        prop::sample::select(containers),
    ]
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
        (any::<usize>(), kind).prop_map(|(at, kind)| Mutation::Retype { at, kind }),
    ]
}

type Path = Vec<Seg>;
#[derive(Clone, Debug)]
enum Seg {
    Key(String),
    Item(usize),
}

fn paths(value: &Value, here: &mut Path, out: &mut Vec<Path>) {
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

fn parent_mut<'a>(root: &'a mut Value, path: &[Seg]) -> Option<&'a mut Value> {
    let mut node = root;
    for seg in path {
        node = match seg {
            Seg::Key(k) => node.get_mut(k.as_str())?,
            Seg::Item(i) => node.get_mut(*i)?,
        };
    }
    Some(node)
}

fn pick_path(event: &Value, pick: usize) -> Option<Path> {
    let mut all = vec![];
    paths(event, &mut vec![], &mut all);
    (!all.is_empty()).then(|| all.swap_remove(pick % all.len()))
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
