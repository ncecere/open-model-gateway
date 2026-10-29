//! HTTP boundary vectors: for every documented query parameter and request body
//! the SCHEMA's verdict on an input must equal the real HANDLER's verdict over
//! HTTP (served router or loopback client), not merely the DTO's serde verdict.
//!
//! * Vectors derive from the artifact's schemas: bounds and bounds +/- 1,
//!   `maxLength + 1`, pattern and format violations, enum members and
//!   non-members, wrong types, empty values, omission and (bodies) null where
//!   not nullable and unknown fields, top level and nested.
//! * The schema is the oracle; the handler "accepts" when it got past input
//!   validation (2xx/3xx, or a domain refusal 404/409) and "refuses" with 400 or
//!   422. Any other status (401/403/413/415/429/5xx) is a harness failure, never
//!   silently counted. A fixture whose status cannot tell input refusal from a
//!   domain refusal supplies its own verdict through [`Probe::decided`].
//! * Executing a full set is recorded per operation; `finish` refuses completed
//!   operations with query parameters or a request body that never ran theirs.
//!
//! Failure messages name method, template, vector kind, the schema-declared
//! parameter name or JSON pointer and the status code: never a sent value.
use super::{
    coverage, request_reach,
    responses::{self, Captured, PositiveFixtures},
};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::collections::BTreeSet;

/// One executed request and the handler's verdict on its input.
pub(crate) struct Probe {
    pub(crate) captured: Captured,
    accepted: Option<bool>,
}
impl From<Captured> for Probe {
    /// The verdict is derived from the status (see the module comment).
    fn from(captured: Captured) -> Self {
        Self {
            captured,
            accepted: None,
        }
    }
}
impl Probe {
    /// An explicit verdict for handlers whose status is ambiguous (for example a
    /// callback that answers 400 both for malformed input and for a refused login).
    /// It replaces only the accept/refuse reading of a status in the expected set
    /// (2xx, 3xx, 400, 404, 409, 422); any other status still fails the run.
    pub(crate) fn decided(captured: Captured, accepted: bool) -> Self {
        Self {
            captured,
            accepted: Some(accepted),
        }
    }
    /// The status must always be one an input verdict can rest on, even when the
    /// fixture decides: 401/403/413/415/429/5xx are never "refused input".
    fn verdict(&self) -> Result<bool> {
        let by_status = match self.captured.status.as_u16() {
            400 | 422 => false,
            200..=399 | 404 | 409 => true,
            other => bail!("unexpected status {other}: not an input verdict"),
        };
        Ok(self.accepted.unwrap_or(by_status))
    }
}

// ---------------------------------------------------------------------------
// Schema walking
// ---------------------------------------------------------------------------

fn resolve<'a>(document: &'a Value, mut node: &'a Value) -> Result<&'a Value> {
    for _ in 0..16 {
        match coverage::follow(document, node)? {
            Some((target, _)) => node = target,
            None => return Ok(node),
        }
    }
    bail!("reference chain too deep")
}

/// Every node along a `$ref` chain, the node itself first: siblings of a `$ref`
/// (`{"$ref": ..., "maxLength": 15}`) are keywords of the instance too, so the
/// validator, `declare` and `reach` see them and the vectors must as well.
fn chain<'a>(document: &'a Value, mut node: &'a Value) -> Result<Vec<&'a Value>> {
    let mut links = vec![node];
    for _ in 0..16 {
        match coverage::follow(document, node)? {
            Some((target, _)) => {
                node = target;
                links.push(node);
            }
            None => return Ok(links),
        }
    }
    bail!("reference chain too deep")
}

fn valid(document: &Value, schema: &Value, instance: &Value) -> Result<bool> {
    Ok(responses::validator(document, schema)?.is_valid(instance))
}

/// The schema nodes that constrain `value`: the node itself, its `allOf` members,
/// the `oneOf`/`anyOf` branches the value matches and the active conditional arm.
fn applicable<'a>(
    document: &'a Value,
    node: &'a Value,
    value: &Value,
    out: &mut Vec<&'a Value>,
    depth: usize,
) -> Result<()> {
    ensure!(depth < 16, "schema nesting too deep");
    // Every link of a `$ref` chain contributes its own keywords, siblings included.
    for node in chain(document, node)? {
        out.push(node);
        for branch in node["allOf"].as_array().into_iter().flatten() {
            applicable(document, branch, value, out, depth + 1)?;
        }
        for keyword in ["oneOf", "anyOf"] {
            for branch in node[keyword].as_array().into_iter().flatten() {
                if valid(document, branch, value)? {
                    applicable(document, branch, value, out, depth + 1)?;
                }
            }
        }
        if let Some(condition) = node.get("if") {
            let keyword = if valid(document, condition, value)? {
                "then"
            } else {
                "else"
            };
            if let Some(branch) = node.get(keyword) {
                applicable(document, branch, value, out, depth + 1)?;
            }
        }
    }
    Ok(())
}

fn first<'a>(nodes: &[&'a Value], key: &str) -> Option<&'a Value> {
    nodes.iter().find_map(|node| node.get(key))
}
/// EVERY distinct value of a bound across the applicable nodes (the base
/// schema and each active `allOf`/`then`/union arm may state a different one):
/// the strictest alone would leave a looser handler cap unprobed.
fn distinct_i64(nodes: &[&Value], key: &str) -> Vec<i64> {
    let all: BTreeSet<i64> = nodes
        .iter()
        .filter_map(|node| node.get(key).and_then(Value::as_i64))
        .collect();
    all.into_iter().collect()
}
fn distinct_u64(nodes: &[&Value], key: &str) -> Vec<u64> {
    let all: BTreeSet<u64> = nodes
        .iter()
        .filter_map(|node| node.get(key).and_then(Value::as_u64))
        .collect();
    all.into_iter().collect()
}
fn types<'a>(nodes: &[&'a Value]) -> BTreeSet<&'a str> {
    let mut out = BTreeSet::new();
    for node in nodes {
        match &node["type"] {
            Value::String(t) => {
                out.insert(t.as_str());
            }
            Value::Array(all) => out.extend(all.iter().filter_map(Value::as_str)),
            _ => {}
        }
    }
    out
}
fn repeat_to(seed: &str, length: usize) -> String {
    let chars: Vec<char> = seed.chars().collect();
    let chars = if chars.is_empty() { vec!['a'] } else { chars };
    (0..length).map(|i| chars[i % chars.len()]).collect()
}

/// Longest string vector (bytes of characters) the harness will send.
const LONGEST: u64 = 100_000;

/// Longest percent-encoded query string the harness sends (the `http` crate
/// refuses request targets over 65534 bytes; the rest is margin for the path).
const MAX_QUERY_BYTES: usize = 60_000;

/// Longest array the harness sends (minItems/maxItems vectors).
const MAX_ITEMS: u64 = 1000;

/// Reviewed exemption on a QUERY PARAMETER or a direct request-body property:
/// the boundary vectors of this location cannot be sent over HTTP (the
/// request-target limit, the harness' string and array caps), so a handler
/// bound the harness cannot probe is accepted on a reviewed reason instead of
/// being skipped silently. Without it an unsendable DECISIVE vector fails.
pub(super) const UNSENDABLE: &str = "x-omg-unsendable-boundary";

/// Reviewed exemption on a string schema whose `pattern` has no length bounds
/// the harness can derive (no `maxLength`/`minLength`/[`MAX_BYTES`] and not one
/// character class under a quantifier): the reason says why the boundary
/// vectors of the pattern are covered some other way. Without it the fixture
/// must supply explicit boundary candidates for the location.
pub(super) const PATTERN_BOUNDARIES: &str = "x-omg-pattern-boundaries";

/// Vector kinds that decide whether the handler enforces a declared length or
/// item bound. If NONE of a kind can be sent for a location the run fails.
const DECISIVE: &[&str] = &[
    "pattern length",
    "minLength",
    "minLength-1",
    "maxLength",
    "maxLength+1",
    "multibyte maxLength",
    "multibyte maxLength+1",
    "maxBytes",
    "maxBytes+1",
    "multibyte maxBytes",
    "multibyte maxBytes+1",
    "minItems",
    "minItems-1",
    "maxItems",
    "maxItems+1",
];

/// Candidate vectors the harness could not send. Decisive ones fail the run
/// unless the location carries [`UNSENDABLE`]; the rest are only counted.
#[derive(Default)]
pub(super) struct Notes {
    /// Decisive kinds with no sendable vector at all: `(kind, location)`.
    pub(super) unsent: Vec<(String, String)>,
    /// Decisive drops excused by a reviewed [`UNSENDABLE`].
    pub(super) exempt: usize,
    /// Non-decisive candidates that were not sent (counted, never silent).
    pub(super) minor: usize,
    /// Locations whose vectors cannot be trusted (unbounded patterns without
    /// candidates, one-sided candidates).
    pub(super) problems: Vec<String>,
}
impl Notes {
    /// `dropped` candidates of one location; `sent` are the ones that WERE sent.
    /// A label is a kind, optionally with the bound it probes (`maxLength+1@64`):
    /// each bound is decisive on its own, so one bound's vectors never excuse
    /// another's.
    pub(super) fn settle<D: AsRef<str>, S: AsRef<str>>(
        &mut self,
        location: &str,
        dropped: &[D],
        sent: &[S],
        exempt: bool,
    ) {
        for label in dropped.iter().map(AsRef::as_ref) {
            let kind = label.split('@').next().unwrap_or(label);
            if sent.iter().any(|s| s.as_ref() == label) || !DECISIVE.contains(&kind) {
                self.minor += 1;
            } else if exempt {
                self.exempt += 1;
            } else if !self.unsent.iter().any(|(k, l)| k == label && l == location) {
                self.unsent.push((label.to_owned(), location.to_owned()));
            }
        }
    }
    pub(super) fn check(&self) -> Result<()> {
        ensure!(self.problems.is_empty(), "{}", self.problems.join("; "));
        ensure!(
            self.unsent.is_empty(),
            "decisive boundary vectors could not be sent ({}): annotate the query parameter or direct body property with {UNSENDABLE} and a reviewed reason, or tighten the bound",
            self.unsent
                .iter()
                .map(|(kind, location)| format!("{kind} at {location}"))
                .collect::<Vec<_>>()
                .join(", ")
        );
        Ok(())
    }
    /// One line, counts only: never a candidate or a location's value.
    fn report(&self, method: &str, template: &str) {
        if self.minor > 0 || self.exempt > 0 {
            eprintln!(
                "{method} {template}: {} non-decisive and {} reviewed-unsendable vector candidates were not sent",
                self.minor, self.exempt
            );
        }
    }
}

/// Reviewed extension on a string schema: the handler limits the UTF-8 BYTES of
/// the value (JSON Schema `maxLength` counts characters, so it can only state a
/// necessary bound). The vector oracle refuses a value over the byte limit and
/// sends multibyte values around it, so a handler that counts characters (or a
/// schema that claims characters for a byte limit) cannot silently pass.
pub(super) const MAX_BYTES: &str = "x-omg-max-bytes";

/// A reviewed byte limit that applies to `instance`, or one of its nested values?
fn bytes_ok(document: &Value, schema: &Value, instance: &Value, depth: usize) -> Result<bool> {
    ensure!(depth < 16, "schema nesting too deep");
    let mut nodes = Vec::new();
    applicable(document, schema, instance, &mut nodes, 0)?;
    match instance {
        Value::String(text) => {
            let over = nodes
                .iter()
                .filter_map(|node| node.get(MAX_BYTES).and_then(Value::as_u64))
                .any(|limit| text.len() as u64 > limit);
            Ok(!over)
        }
        Value::Object(map) => {
            for (name, child) in map {
                // Properties AND typed map values (`patternProperties` /
                // `additionalProperties`) carry byte limits too.
                if let Some(child_schema) = member_schema(&nodes, name)?
                    && !bytes_ok(document, &child_schema, child, depth + 1)?
                {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        Value::Array(items) => {
            for (i, child) in items.iter().enumerate() {
                // EVERY element: a positional schema, else the homogeneous `items`.
                if let Some(item_schema) = element_schema(&nodes, i)
                    && !bytes_ok(document, &item_schema, child, depth + 1)?
                {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        _ => Ok(true),
    }
}

/// The schema's verdict: JSON Schema validity plus the reviewed byte limits.
pub(super) fn accepts(document: &Value, schema: &Value, instance: &Value) -> Result<bool> {
    Ok(valid(document, schema, instance)? && bytes_ok(document, schema, instance, 0)?)
}

/// `(min, max)` repetitions of a SIMPLE anchored pattern: one character class,
/// escape or literal under `{n}`, `{m,n}`, `{m,}`, `+`, `*` or `?`. Anything
/// else (groups, alternation, sequences) yields `None`.
fn simple_bounds(pattern: &str) -> Option<(usize, Option<usize>)> {
    let body = pattern.strip_prefix('^')?.strip_suffix('$')?;
    let rest = if body.starts_with('[') {
        let mut escaped = false;
        let mut end = None;
        for (i, c) in body.char_indices().skip(1) {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == ']' && i > 1 {
                end = Some(i);
                break;
            }
        }
        &body[end? + 1..]
    } else if body.starts_with('\\') {
        body.get(2..)?
    } else {
        let first = body.chars().next()?;
        if "()|.^$*+?{}[]".contains(first) {
            return None;
        }
        &body[first.len_utf8()..]
    };
    match rest {
        "+" => Some((1, None)),
        "*" => Some((0, None)),
        "?" => Some((0, Some(1))),
        quantifier => {
            let inner = quantifier.strip_prefix('{')?.strip_suffix('}')?;
            match inner.split_once(',') {
                None => {
                    let n = inner.parse().ok()?;
                    Some((n, Some(n)))
                }
                Some((low, "")) => Some((low.parse().ok()?, None)),
                Some((low, high)) => Some((low.parse().ok()?, Some(high.parse().ok()?))),
            }
        }
    }
}

/// Candidates around a pattern: shape violations, control characters,
/// multibyte values and, for simple repetition patterns, the length boundaries
/// m-1, m, n and n+1 built from the characters of the valid seed.
pub(super) fn pattern_candidates(
    pattern: &str,
    seed: &str,
    dropped: &mut Vec<String>,
) -> (Vec<String>, Vec<(usize, String)>) {
    let mut replaced = seed.chars();
    replaced.next();
    let mut out = vec![
        format!("{seed}!"),
        format!("!{seed}"),
        format!("!{}", replaced.as_str()),
        seed.to_uppercase(),
        seed.to_lowercase(),
        " ".to_owned(),
        "~".to_owned(),
        "\u{e9}".to_owned(),
    ];
    for control in ['\n', '\0', '\u{7f}', '\u{85}'] {
        out.push(control.to_string());
        out.push(format!("{seed}{control}"));
        out.push(format!("{control}{seed}"));
        out.push(format!("a{control}b"));
    }
    for glyph in ["\u{e9}", "\u{2713}", "\u{1F600}"] {
        out.push(glyph.to_owned());
        out.push(format!("{seed}{glyph}"));
    }
    // The derived length boundaries are DECISIVE for the pattern: each is
    // labelled by its length, so one unsendable boundary is never excused by
    // another that was sent.
    let mut lengths = Vec::new();
    if let Some((min, max)) = simple_bounds(pattern) {
        let wanted = [min.checked_sub(1), Some(min), max, max.map(|n| n + 1)];
        for length in wanted.into_iter().flatten() {
            if (length as u64) <= LONGEST {
                lengths.push((length, repeat_to(seed, length)));
            } else {
                dropped.push(format!("pattern length@{length}"));
            }
        }
    }
    (out, lengths)
}

// ---------------------------------------------------------------------------
// Meaningful decisive vectors
// ---------------------------------------------------------------------------
//
// A length or item bound counts as probed only by a PAIR of vectors: one AT the
// limit that the schema accepts, one just PAST it that the schema refuses
// BECAUSE OF THAT BOUND (the same candidate is accepted once that one keyword is
// removed). Candidates are built from characters (or elements) that satisfy the
// other constraints, so a `maxLength: 64` beside `^sk-[a-z0-9]+$` is probed with
// an `sk-...` value of 64 and 65 characters, not with text the pattern refuses
// anyway. When another constraint ALONE refuses every candidate past the limit
// (`^[a-z]{1,8}$` beside `maxLength: 8`, a `format: uuid`), the bound is IMPLIED:
// the pair is still sent, but no vector could tell the bound from the other
// constraint. A bound with no such pair at all is reported (and fails unless the
// location carries `x-omg-unsendable-boundary`).

/// Decisive vectors of one location, by label (`kind@bound`).
#[derive(Default)]
struct Evidence {
    labelled: Vec<(String, &'static str, Value)>,
    dropped: Vec<String>,
}

/// An at-limit vector the schema accepts and the vector just past it.
#[derive(Clone)]
struct Pair {
    at: Value,
    over: Value,
}

fn label(kind: &str, bound: u64) -> String {
    format!("{kind}@{bound}")
}

impl Evidence {
    /// Record the pair found for one bound (or why there is none). A bound
    /// another, stricter bound of the same kind dominates cannot be probed on its
    /// own: its failure is only counted.
    fn settle(
        &mut self,
        kinds: (&'static str, Option<&'static str>),
        bound: u64,
        found: Option<Pair>,
        dominated: bool,
    ) {
        let all = [Some(kinds.0), kinds.1];
        match found {
            Some(pair) => {
                self.labelled
                    .push((label(kinds.0, bound), kinds.0, pair.at));
                if let Some(over) = kinds.1 {
                    self.labelled.push((label(over, bound), over, pair.over));
                }
            }
            None => {
                for kind in all.into_iter().flatten() {
                    self.dropped.push(if dominated {
                        format!("dominated {kind}@{bound}")
                    } else {
                        label(kind, bound)
                    });
                }
            }
        }
    }
}

/// The schema with every `key: bound` removed (the references it follows
/// included): what the schema would say without that one bound.
fn strip(node: &mut Value, key: &str, bound: &Value) {
    match node {
        Value::Object(map) => {
            if map.get(key) == Some(bound) {
                map.remove(key);
            }
            for child in map.values_mut() {
                strip(child, key, bound);
            }
        }
        Value::Array(items) => {
            for child in items {
                strip(child, key, bound);
            }
        }
        _ => {}
    }
}

fn relaxed(
    document: &Value,
    schema: &Value,
    key: &str,
    bound: u64,
) -> Result<std::sync::Arc<jsonschema::Validator>> {
    static CACHE: std::sync::OnceLock<
        std::sync::Mutex<std::collections::BTreeMap<String, std::sync::Arc<jsonschema::Validator>>>,
    > = std::sync::OnceLock::new();
    let bound = json!(bound);
    let compile = || -> Result<std::sync::Arc<jsonschema::Validator>> {
        let mut document = document.clone();
        strip(&mut document, key, &bound);
        let mut schema = schema.clone();
        strip(&mut schema, key, &bound);
        // A fresh document is never the artifact: this compiles without caching.
        responses::validator(&document, &schema)
    };
    // Only the immutable committed artifact is cached (by value of the request).
    if !std::ptr::eq(document, responses::artifact()) {
        return compile();
    }
    let cache_key = format!("{key}={bound} {}", serde_json::to_string(schema)?);
    if let Some(found) = CACHE
        .get_or_init(Default::default)
        .lock()
        .unwrap()
        .get(&cache_key)
    {
        return Ok(found.clone());
    }
    let validator = compile()?;
    CACHE
        .get_or_init(Default::default)
        .lock()
        .unwrap()
        .insert(cache_key, validator.clone());
    Ok(validator)
}

/// The first candidate pair the schema accepts at the limit and refuses past
/// it, preferring one the bound ALONE refuses (`relaxed` accepts it). `None`
/// when no candidate satisfies the schema at the limit.
fn find_pair(
    document: &Value,
    schema: &Value,
    relaxed: Option<&jsonschema::Validator>,
    candidates: impl Iterator<Item = (Value, Value)>,
) -> Result<Option<Pair>> {
    let mut implied = None;
    for (at, over) in candidates {
        if !accepts(document, schema, &at)? || accepts(document, schema, &over)? {
            continue;
        }
        let alone = match relaxed {
            Some(relaxed) => relaxed.is_valid(&over),
            None => valid(document, schema, &over)?,
        };
        if alone {
            return Ok(Some(Pair { at, over }));
        }
        implied.get_or_insert(Pair { at, over });
    }
    Ok(implied)
}

const GLYPHS: [&str; 3] = ["\u{e9}", "\u{2713}", "\u{1F600}"];

/// Printable ASCII, the last resort for a fill character a pattern allows.
fn sweep() -> impl Iterator<Item = char> {
    ' '..='~'
}

/// Strategies for a fill of `length` characters: the seed repeated, then the
/// seed (truncated) padded with each of its own characters, then with every
/// printable ASCII character. The pad keeps the seed's prefix and alphabet.
fn strategy_count(seed: &str) -> usize {
    1 + seed.chars().collect::<BTreeSet<_>>().len() + sweep().count()
}
fn fill(seed: &str, length: usize, strategy: usize) -> Option<String> {
    if strategy == 0 {
        return Some(repeat_to(seed, length));
    }
    let mut distinct: Vec<char> = Vec::new();
    for c in seed.chars() {
        if !distinct.contains(&c) {
            distinct.push(c);
        }
    }
    let pad = match strategy - 1 {
        n if n < distinct.len() => distinct[n],
        n => sweep().nth(n - distinct.len())?,
    };
    let head: String = seed.chars().take(length).collect();
    let rest = length - head.chars().count();
    Some(format!("{head}{}", pad.to_string().repeat(rest)))
}
fn ascii_pairs(seed: &str, at: usize, over: usize) -> impl Iterator<Item = (Value, Value)> + '_ {
    (0..strategy_count(seed))
        .filter_map(move |s| Some((json!(fill(seed, at, s)?), json!(fill(seed, over, s)?))))
}

/// `head` (a prefix of the seed) then `glyph` repeated to `chars` characters.
fn glyph_value(seed: &str, head: usize, glyph: &str, chars: usize) -> Option<String> {
    (chars > head).then(|| {
        let head: String = seed.chars().take(head).collect();
        format!("{head}{}", glyph.repeat(chars - head.chars().count()))
    })
}
fn heads(seed: &str) -> std::ops::RangeInclusive<usize> {
    0..=seed.chars().count().min(8)
}
/// Pairs of `chars` and `chars + 1` characters ending in `glyph`s.
fn glyph_pairs<'a>(seed: &'a str, chars: usize) -> impl Iterator<Item = (Value, Value)> + 'a {
    GLYPHS.into_iter().flat_map(move |glyph| {
        heads(seed).filter_map(move |head| {
            Some((
                json!(glyph_value(seed, head, glyph, chars)?),
                json!(glyph_value(seed, head, glyph, chars + 1)?),
            ))
        })
    })
}
/// A value of exactly `bytes` UTF-8 bytes: `head`, `glyph`s, then ASCII `pad`.
fn byte_value(seed: &str, head: usize, glyph: &str, pad: char, bytes: usize) -> Option<String> {
    let head: String = seed.chars().take(head).collect();
    let rest = bytes.checked_sub(head.len())?;
    let glyphs = rest / glyph.len();
    (glyphs > 0).then(|| {
        format!(
            "{head}{}{}",
            glyph.repeat(glyphs),
            pad.to_string().repeat(rest - glyphs * glyph.len())
        )
    })
}
/// Pairs of exactly `bytes` and `bytes + 1` bytes, multibyte, padded with ASCII
/// so that `>=` and `>` are told apart.
fn byte_pairs<'a>(seed: &'a str, bytes: usize) -> impl Iterator<Item = (Value, Value)> + 'a {
    let mut pads: Vec<char> = seed.chars().filter(char::is_ascii).collect();
    pads.extend(['a', '0', 'A', 'x']);
    GLYPHS.into_iter().flat_map(move |glyph| {
        let pads = pads.clone();
        heads(seed).flat_map(move |head| {
            pads.clone().into_iter().filter_map(move |pad| {
                Some((
                    json!(byte_value(seed, head, glyph, pad, bytes)?),
                    json!(byte_value(seed, head, glyph, pad, bytes + 1)?),
                ))
            })
        })
    })
}

/// Can the schema hold a multibyte value at all (at its shortest length)?
fn multibyte_possible(
    document: &Value,
    schema: &Value,
    seed: &str,
    shortest: usize,
    longest: usize,
) -> Result<bool> {
    for glyph in GLYPHS {
        for head in heads(seed) {
            let chars = shortest.max(head + 1);
            if chars <= longest
                && let Some(candidate) = glyph_value(seed, head, glyph, chars)
                && accepts(document, schema, &json!(candidate))?
            {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

/// The length vectors of a string location, one pair per DISTINCT bound.
fn string_bounds(
    document: &Value,
    schema: &Value,
    nodes: &[&Value],
    seed: &str,
    out: &mut Vec<(&'static str, Value)>,
    evidence: &mut Evidence,
) -> Result<()> {
    let mins = distinct_u64(nodes, "minLength");
    let maxs = distinct_u64(nodes, "maxLength");
    let byte_limits = distinct_u64(nodes, MAX_BYTES);
    for &min in mins.iter().filter(|min| **min > 0) {
        let dominated = Some(&min) != mins.last();
        if min > LONGEST {
            evidence.settle(("minLength", Some("minLength-1")), min, None, dominated);
            continue;
        }
        let validator = relaxed(document, schema, "minLength", min)?;
        let found = find_pair(
            document,
            schema,
            Some(&validator),
            ascii_pairs(seed, min as usize, min as usize - 1),
        )?;
        if found.is_none() {
            out.push(("minLength", json!(repeat_to(seed, min as usize))));
            out.push(("minLength-1", json!(repeat_to(seed, min as usize - 1))));
        }
        evidence.settle(("minLength", Some("minLength-1")), min, found, dominated);
    }
    let shortest = mins.last().copied().unwrap_or(0) as usize;
    for &max in &maxs {
        let dominated = Some(&max) != maxs.first();
        if max >= LONGEST {
            evidence.settle(("maxLength", Some("maxLength+1")), max, None, dominated);
        } else {
            let validator = relaxed(document, schema, "maxLength", max)?;
            let found = find_pair(
                document,
                schema,
                Some(&validator),
                ascii_pairs(seed, max as usize, max as usize + 1),
            )?;
            if found.is_none() {
                out.push(("maxLength", json!(repeat_to(seed, max as usize))));
                out.push(("maxLength+1", json!(repeat_to(seed, max as usize + 1))));
            }
            evidence.settle(("maxLength", Some("maxLength+1")), max, found, dominated);
        }
        // Characters, not bytes: `max` multibyte characters are within maxLength
        // however many bytes they take. A byte limit below two bytes per
        // character, or a schema that refuses multibyte text, leaves nothing
        // to tell apart (counted, not decisive).
        let kinds = ("multibyte maxLength", Some("multibyte maxLength+1"));
        if byte_limits.iter().any(|limit| *limit < 2 * max)
            || !multibyte_possible(document, schema, seed, shortest, max as usize)?
        {
            evidence.dropped.push("multibyte not applicable".to_owned());
            // Still sent when small enough: the handler must agree with the
            // schema (a byte limit refuses `max` two-byte characters).
            if max <= 20_000 {
                for glyph in ["\u{e9}", "\u{1F600}"] {
                    out.push(("multibyte maxLength", json!(glyph.repeat(max as usize))));
                    out.push((
                        "multibyte maxLength+1",
                        json!(glyph.repeat(max as usize + 1)),
                    ));
                }
            }
        } else if max > 20_000 {
            evidence.settle(kinds, max, None, dominated);
        } else {
            let validator = relaxed(document, schema, "maxLength", max)?;
            let found = find_pair(
                document,
                schema,
                Some(&validator),
                glyph_pairs(seed, max as usize),
            )?;
            evidence.settle(kinds, max, found, dominated);
        }
    }
    // A reviewed byte limit (every distinct one): ASCII and multibyte values of
    // EXACTLY the limit and of one byte more.
    for &limit in &byte_limits {
        let dominated = Some(&limit) != byte_limits.first();
        if limit >= LONGEST {
            evidence.settle(("maxBytes", Some("maxBytes+1")), limit, None, dominated);
            evidence.settle(
                ("multibyte maxBytes", Some("multibyte maxBytes+1")),
                limit,
                None,
                dominated,
            );
            continue;
        }
        let ascii = find_pair(
            document,
            schema,
            None,
            ascii_pairs(seed, limit as usize, limit as usize + 1)
                .filter(|(at, _)| at.as_str().is_some_and(|t| t.len() == limit as usize)),
        )?;
        let possible = multibyte_possible(document, schema, seed, shortest, usize::MAX)?;
        let wide = if possible {
            find_pair(document, schema, None, byte_pairs(seed, limit as usize))?
        } else {
            None
        };
        let exact = ascii.or_else(|| wide.clone());
        evidence.settle(("maxBytes", Some("maxBytes+1")), limit, exact, dominated);
        if possible {
            evidence.settle(
                ("multibyte maxBytes", Some("multibyte maxBytes+1")),
                limit,
                wide,
                dominated,
            );
        }
    }
    Ok(())
}

const STRADDLE: &str = "supplied candidates must straddle the boundary: include a value the schema accepts and one it refuses that differs from it by ONE character added, removed or changed";

/// Do the `(candidate, schema verdict)` pairs hold an accepted value and a
/// refused one within edit distance 1 of it? Two candidates far apart (`0` and
/// sixteen digits) prove nothing about WHERE the boundary lies.
fn straddles(verdicts: &[(String, bool)]) -> bool {
    fn one_edit(a: &str, b: &str) -> bool {
        let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
        let (short, long) = if a.len() <= b.len() {
            (&a, &b)
        } else {
            (&b, &a)
        };
        match long.len() - short.len() {
            0 => short.iter().zip(long).filter(|(x, y)| x != y).count() == 1,
            1 => (0..long.len()).any(|skip| {
                long.iter()
                    .enumerate()
                    .filter(|(i, _)| *i != skip)
                    .map(|(_, c)| c)
                    .eq(short.iter())
            }),
            _ => false,
        }
    }
    verdicts.iter().any(|(accepted, ok)| {
        *ok && verdicts
            .iter()
            .any(|(refused, ok)| !*ok && one_edit(accepted, refused))
    })
}

/// The candidates of one location and the decisive kinds it could not produce.
struct Mutations {
    vectors: Vec<(&'static str, Value)>,
    /// Decisive vectors of one bound, by label (`maxLength+1@64`): built from
    /// candidates that satisfy the other constraints, see [`length_pair`].
    labelled: Vec<(String, &'static str, Value)>,
    /// Labels (or kinds) the harness could not send or build meaningfully.
    dropped: Vec<String>,
    /// The location has a pattern with no derivable length bounds, no reviewed
    /// [`PATTERN_BOUNDARIES`] and no supplied candidates.
    needs_candidates: bool,
    /// [`PATTERN_BOUNDARIES`] is present but nothing needs it any more.
    stale_pattern_boundaries: bool,
}

/// Every `const` value and `enum` member a schema declares, including those of
/// ALL its union and conjunction branches (not only the ones the current value
/// matches), so `oneOf: [{const: "month"}, {const: "day"}]` offers "day" beside "month".
fn declared_values(
    document: &Value,
    node: &Value,
    out: &mut Vec<Value>,
    depth: usize,
) -> Result<()> {
    ensure!(depth < 16, "schema nesting too deep");
    for node in chain(document, node)? {
        let mut add = |value: &Value| {
            if !out.contains(value) {
                out.push(value.clone());
            }
        };
        if let Some(constant) = node.get("const") {
            add(constant);
        }
        for member in node["enum"].as_array().into_iter().flatten() {
            add(member);
        }
        for keyword in ["oneOf", "anyOf", "allOf"] {
            for branch in node[keyword].as_array().into_iter().flatten() {
                declared_values(document, branch, out, depth + 1)?;
            }
        }
    }
    Ok(())
}

/// Candidate replacement values for one location, by the constraints that apply.
/// `query` candidates are rendered as strings; JSON wrong types do not exist there.
fn scalar_mutations(
    document: &Value,
    schema: &Value,
    nodes: &[&Value],
    value: &Value,
    query: bool,
    candidates: usize,
) -> Result<Mutations> {
    let mut out: Vec<(&'static str, Value)> = Vec::new();
    let mut evidence = Evidence::default();
    let mut dropped: Vec<String> = Vec::new();
    let mut needs_candidates = false;
    let mut stale_pattern_boundaries = false;
    let kinds = types(nodes);
    if !query {
        for wrong in [
            json!(null),
            json!(true),
            json!(1),
            json!("wrong"),
            json!([]),
            json!({}),
        ] {
            // A candidate of an ALLOWED JSON type is a value vector, not a type vector.
            let allowed = match &wrong {
                Value::Null => false, // always probed: nullable fields must accept it
                Value::Bool(_) => kinds.contains("boolean"),
                Value::Number(_) => kinds.contains("integer") || kinds.contains("number"),
                Value::String(_) => kinds.contains("string"),
                Value::Array(_) => kinds.contains("array"),
                Value::Object(_) => kinds.contains("object"),
            };
            if !allowed {
                out.push((
                    if wrong.is_null() {
                        "null"
                    } else {
                        "wrong type"
                    },
                    wrong,
                ));
            }
        }
        if kinds.contains("string") {
            out.push(("empty", json!("")));
        }
    } else {
        if kinds.contains("integer") {
            for raw in ["abc", "true", "9999999999999999999999"] {
                out.push(("wrong type", json!(raw)));
            }
            out.push(("non-integer", json!("1.5")));
        }
        if kinds.contains("number") {
            out.push(("wrong type", json!("abc")));
        }
        if kinds.contains("boolean") {
            for raw in ["yes", "1", "TRUE"] {
                out.push(("wrong type", json!(raw)));
            }
        }
        out.push(("empty", json!("")));
    }
    // Flip booleans and consts, so a value that is valid alone but not beside its
    // siblings (`ids` together with `all: true`) reaches the handler.
    if let Value::Bool(flag) = value {
        out.push(("boolean flip", json!(!flag)));
    }
    for node in nodes {
        if let Some(constant) = node.get("const") {
            let other = match constant {
                Value::String(text) => json!(format!("{text}~")),
                Value::Number(n) => json!(n.as_i64().map_or(0, |n| n.saturating_add(1))),
                Value::Bool(flag) => json!(!flag),
                _ => json!("~not-the-constant~"),
            };
            if other != *value {
                out.push(("const flip", other));
            }
        }
    }
    // Members of every applicable enum AND the consts/enums of sibling union
    // branches the value does not match (a handler may refuse one of them).
    let mut members: Vec<Value> = Vec::new();
    for node in nodes {
        for member in node["enum"].as_array().into_iter().flatten() {
            if !members.contains(member) {
                members.push(member.clone());
            }
        }
    }
    declared_values(document, schema, &mut members, 0)?;
    if !members.is_empty() {
        out.push(("enum non-member", json!("~not-a-member~")));
        for member in members.iter().filter(|m| !m.is_null() && *m != value) {
            out.push(("enum member", member.clone()));
        }
    }
    // Numeric bounds (every distinct one), each side and its neighbour.
    if kinds.contains("integer") || kinds.contains("number") {
        let mut push = |kind: &'static str, n: i64| out.push((kind, json!(n)));
        for min in distinct_i64(nodes, "minimum") {
            push("minimum", min);
            push("minimum-1", min.saturating_sub(1));
        }
        for max in distinct_i64(nodes, "maximum") {
            push("maximum", max);
            push("maximum+1", max.saturating_add(1));
        }
        for min in distinct_i64(nodes, "exclusiveMinimum") {
            push("exclusiveMinimum", min);
            push("exclusiveMinimum+1", min.saturating_add(1));
        }
        for max in distinct_i64(nodes, "exclusiveMaximum") {
            push("exclusiveMaximum", max);
            push("exclusiveMaximum-1", max.saturating_sub(1));
        }
        if !query && kinds.contains("integer") {
            out.push(("non-integer", json!(1.5)));
        }
    }
    // String bounds are derived from the VALID value so the alphabet stays valid.
    if kinds.contains("string") {
        let seed = value.as_str().unwrap_or("a");
        string_bounds(document, schema, nodes, seed, &mut out, &mut evidence)?;
        dropped.append(&mut evidence.dropped);
        if let Some(pattern) = first(nodes, "pattern").and_then(Value::as_str) {
            // A pattern is bounded by its own shape, or by `maxLength` /
            // `x-omg-max-bytes` (a `minLength` alone bounds nothing from above),
            // or by a reviewed reason.
            let bounded = simple_bounds(pattern).is_some()
                || !distinct_u64(nodes, "maxLength").is_empty()
                || nodes.iter().any(|node| {
                    node.get(MAX_BYTES).is_some() || node.get(PATTERN_BOUNDARIES).is_some()
                });
            needs_candidates = !bounded && candidates == 0;
            // The reviewed reason is for a pattern whose bounds cannot be
            // derived and for which no candidates are supplied; once the
            // pattern, a maxLength/maxBytes or candidates cover it, it is stale.
            let own = simple_bounds(pattern).is_some()
                || !distinct_u64(nodes, "maxLength").is_empty()
                || nodes.iter().any(|node| node.get(MAX_BYTES).is_some());
            stale_pattern_boundaries = nodes
                .iter()
                .any(|node| node.get(PATTERN_BOUNDARIES).is_some())
                && (own || candidates > 0);
            let (shapes, lengths) = pattern_candidates(pattern, seed, &mut dropped);
            for (length, candidate) in lengths {
                let kind = if accepts(document, schema, &json!(candidate))? {
                    "pattern candidate"
                } else {
                    "pattern violation"
                };
                evidence.labelled.push((
                    format!("pattern length@{length}"),
                    kind,
                    json!(candidate),
                ));
            }
            for candidate in shapes {
                // Genuine violations are the pattern-violation vectors; the rest
                // (multibyte text, say) must be accepted by both sides.
                let kind = if accepts(document, schema, &json!(candidate))? {
                    "pattern candidate"
                } else {
                    "pattern violation"
                };
                out.push((kind, json!(candidate)));
            }
        }
        if let Some(format) = first(nodes, "format").and_then(Value::as_str) {
            let bad: &[&str] = match format {
                "date" => &["2024-02-30", "2024-13-01", "not-a-date"],
                "date-time" => &["yesterday", "2024-01-01"],
                "uuid" => &["not-a-uuid"],
                "email" => &["not-an-email"],
                "uri" => &["not a uri"],
                _ => &[],
            };
            for candidate in bad {
                out.push(("format violation", json!(candidate)));
            }
        }
        if query {
            for candidate in ["a b", "\u{e9}\u{2713}", "a&b=c#d"] {
                out.push(("special characters", json!(candidate)));
            }
        }
    }
    Ok(Mutations {
        vectors: out,
        labelled: evidence.labelled,
        dropped,
        needs_candidates,
        stale_pattern_boundaries,
    })
}

// ---------------------------------------------------------------------------
// Query vectors
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Edit {
    Omit,
    Set(String),
}
#[derive(Clone, Debug)]
pub(super) struct QueryCase {
    pub(super) param: String,
    pub(super) kind: &'static str,
    pub(super) edit: Edit,
    pub(super) accept: bool,
}

fn render(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

/// A raw query string value as the typed JSON the schema describes (query
/// values always arrive as strings): canonical integers, numbers and booleans only.
fn coerce(kinds: &BTreeSet<&str>, raw: &str) -> Value {
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    let unsigned = raw.strip_prefix('-').unwrap_or(raw);
    let canonical = digits(unsigned) && (unsigned == "0" || !unsigned.starts_with('0'));
    if kinds.contains("integer") && canonical && raw != "-0" {
        if let Ok(n) = raw.parse::<i64>() {
            return json!(n);
        }
        if let Ok(n) = raw.parse::<u64>() {
            return json!(n);
        }
    }
    if kinds.contains("number") {
        let (whole, fraction) = unsigned.split_once('.').unwrap_or((unsigned, "1"));
        if digits(whole) && digits(fraction) && (whole == "0" || !whole.starts_with('0')) {
            if let Ok(n) = raw.parse::<i64>() {
                return json!(n);
            }
            if let Ok(n) = raw.parse::<f64>() {
                return json!(n);
            }
        }
    }
    if kinds.contains("boolean") && (raw == "true" || raw == "false") {
        return json!(raw == "true");
    }
    json!(raw)
}

/// `(name, required, schema, unsendable-boundary exemption)` per query parameter.
type QueryParameter<'a> = (String, bool, &'a Value, bool);

fn query_parameters<'a>(
    document: &'a Value,
    operation: &'a Value,
) -> Result<Vec<QueryParameter<'a>>> {
    let mut out = Vec::new();
    for parameter in operation["parameters"].as_array().into_iter().flatten() {
        let parameter = resolve(document, parameter)?;
        if parameter["in"] == "query" {
            out.push((
                parameter["name"]
                    .as_str()
                    .context("query parameter needs a name")?
                    .to_owned(),
                parameter["required"] == true,
                parameter
                    .get("schema")
                    .context("query parameter needs a schema")?,
                parameter.get(UNSENDABLE).is_some(),
            ));
        }
    }
    Ok(out)
}

pub(super) struct QueryPlan {
    pub(super) cases: Vec<QueryCase>,
    pub(super) notes: Notes,
}

/// Every query vector of an operation. `base` is one VALID request; `seeds` give a
/// valid value for documented OPTIONAL parameters the base request does not carry
/// (a vector for such a parameter ADDS it to the base request). Fails when a
/// decisive boundary vector cannot be sent (see [`UNSENDABLE`]).
pub(super) fn query_cases(
    document: &Value,
    operation: &Value,
    base: &[(String, String)],
    seeds: &[(String, String)],
) -> Result<Vec<QueryCase>> {
    Ok(query_plan(document, operation, base, seeds, &[])?.cases)
}

/// The request's query string with `name` set (replaced, or added) to `raw`.
fn with_param(base: &[(String, String)], name: &str, raw: &str) -> Vec<(String, String)> {
    let mut pairs = base.to_vec();
    if pairs.iter().any(|(n, _)| n == name) {
        for pair in pairs.iter_mut().filter(|(n, _)| n == name) {
            pair.1 = raw.to_owned();
        }
    } else {
        pairs.push((name.to_owned(), raw.to_owned()));
    }
    pairs
}

/// `candidates` are `(parameter, value)` pairs the fixture supplies for a
/// parameter whose pattern has no derivable bounds; each is sent in place of
/// the valid value and judged by the schema.
pub(super) fn query_plan(
    document: &Value,
    operation: &Value,
    base: &[(String, String)],
    seeds: &[(String, String)],
    candidates: &[(String, String)],
) -> Result<QueryPlan> {
    let mut notes = Notes::default();
    let parameters = query_parameters(document, operation)?;
    ensure!(
        !parameters.is_empty(),
        "the operation documents no query parameters"
    );
    let mut out = Vec::new();
    for (name, required, schema, unsendable) in &parameters {
        let raw = base
            .iter()
            .chain(seeds)
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
            .context("every documented query parameter needs a valid base or seed value")?;
        let mut nodes = Vec::new();
        let kinds = {
            applicable(document, schema, &json!(raw), &mut nodes, 0)?;
            types(&nodes)
        };
        ensure!(
            valid(document, schema, &coerce(&kinds, raw))?,
            "the valid base value of a query parameter must satisfy its schema"
        );
        let mut seen = BTreeSet::from([raw.to_owned()]);
        if base.iter().any(|(n, _)| n == name) {
            out.push(QueryCase {
                param: name.clone(),
                kind: "omitted",
                edit: Edit::Omit,
                accept: !required,
            });
        }
        let supplied: Vec<&str> = candidates
            .iter()
            .filter(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
            .collect();
        let mut mutations = scalar_mutations(
            document,
            schema,
            &nodes,
            &coerce(&kinds, raw),
            true,
            supplied.len(),
        )?;
        if mutations.needs_candidates {
            notes.problems.push(format!(
                "query parameter {name}: the pattern has no derivable length bounds; supply boundary candidates or annotate it with {PATTERN_BOUNDARIES}"
            ));
        }
        if mutations.stale_pattern_boundaries {
            notes.problems.push(format!(
                "query parameter {name}: {PATTERN_BOUNDARIES} is stale: the pattern, a maxLength or the supplied candidates already cover its boundaries; delete it"
            ));
        }
        if !supplied.is_empty() {
            let verdicts = supplied
                .iter()
                .map(|v| {
                    Ok((
                        v.to_string(),
                        accepts(document, schema, &coerce(&kinds, v))?,
                    ))
                })
                .collect::<Result<Vec<_>>>()?;
            if !straddles(&verdicts) {
                notes
                    .problems
                    .push(format!("query parameter {name}: {STRADDLE}"));
            }
            mutations
                .vectors
                .extend(supplied.iter().map(|v| ("supplied candidate", json!(v))));
        }
        let mut dropped = mutations.dropped;
        let mut sent: Vec<String> = Vec::new();
        let all = mutations.labelled.into_iter().chain(
            mutations
                .vectors
                .into_iter()
                .map(|(kind, value)| (kind.to_owned(), kind, value)),
        );
        for (label, kind, value) in all {
            let raw = render(&value);
            // A request target is limited to 65534 bytes by the HTTP stack: the
            // REAL percent-encoded query string (not a worst-case estimate) must fit.
            if encode(&with_param(base, name, &raw)).len() > MAX_QUERY_BYTES {
                dropped.push(label);
                continue;
            }
            sent.push(label);
            if seen.insert(raw.clone()) {
                let accept = accepts(document, schema, &coerce(&kinds, &raw))?;
                out.push(QueryCase {
                    param: name.clone(),
                    kind,
                    edit: Edit::Set(raw),
                    accept,
                });
            }
        }
        let exempt = *unsendable;
        let before = notes.exempt;
        notes.settle(name, &dropped, &sent, exempt);
        if exempt && notes.exempt == before {
            notes.problems.push(format!(
                "query parameter {name}: {UNSENDABLE} is stale: every decisive vector of this parameter can be sent; delete it"
            ));
        }
    }
    for name in base.iter().chain(seeds).map(|(n, _)| n) {
        ensure!(
            parameters.iter().any(|(p, ..)| p == name),
            "the valid base request carries an undocumented query parameter"
        );
    }
    for (name, _) in candidates {
        ensure!(
            parameters.iter().any(|(p, ..)| p == name),
            "a supplied candidate names an undocumented query parameter"
        );
    }
    notes.check()?;
    Ok(QueryPlan { cases: out, notes })
}

pub(super) fn encode(pairs: &[(String, String)]) -> String {
    // Percent-encode everything outside the RFC 3986 unreserved set.
    fn escape(text: &str) -> String {
        text.bytes()
            .map(|b| match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                    char::from(b).to_string()
                }
                _ => format!("%{b:02X}"),
            })
            .collect()
    }
    pairs
        .iter()
        .map(|(name, value)| format!("{}={}", escape(name), escape(value)))
        .collect::<Vec<_>>()
        .join("&")
}

fn same_operation(captured: &Captured, method: &str, template: &str) -> Result<()> {
    ensure!(
        captured.method == method && captured.template.as_deref() == Some(template),
        "a vector request reached a different operation than {method} {template}"
    );
    Ok(())
}

fn mismatch(
    method: &str,
    template: &str,
    kind: &str,
    location: &str,
    schema_accepts: bool,
    probe: &Probe,
    handler_accepts: bool,
) -> anyhow::Error {
    let word = |accepts: bool| if accepts { "accepts" } else { "refuses" };
    anyhow::anyhow!(
        "{method} {template}: {kind} vector at {location}: schema {} but handler {} (status {})",
        word(schema_accepts),
        word(handler_accepts),
        probe.captured.status.as_u16()
    )
}

/// Run every query vector; returns every exchange (so the caller can observe them).
pub(super) async fn run_query<C>(
    document: &Value,
    method: &str,
    template: &str,
    seeds: &[(String, String)],
    fresh: &impl AsyncFn() -> (C, Vec<(String, String)>),
    send: &impl AsyncFn(&C, String) -> Probe,
) -> Result<Vec<Captured>> {
    run_query_with(document, method, template, seeds, &[], fresh, send).await
}

/// [`run_query`] with fixture-supplied boundary `candidates` (see [`query_plan`]).
pub(super) async fn run_query_with<C>(
    document: &Value,
    method: &str,
    template: &str,
    seeds: &[(String, String)],
    candidates: &[(String, String)],
    fresh: &impl AsyncFn() -> (C, Vec<(String, String)>),
    send: &impl AsyncFn(&C, String) -> Probe,
) -> Result<Vec<Captured>> {
    let operation = responses::operation(document, method, template)?;
    let (_, base) = fresh().await;
    let plan = query_plan(document, operation, &base, seeds, candidates)
        .with_context(|| format!("{method} {template}"))?;
    plan.notes.report(method, template);
    let cases = plan.cases;
    let mut exchanges = Vec::new();
    // The all-valid request is the positive control for every other vector.
    let mut plan: Vec<(Option<&QueryCase>, &str, String, bool)> =
        vec![(None, "valid", "(all)".into(), true)];
    for case in &cases {
        plan.push((Some(case), case.kind, case.param.clone(), case.accept));
    }
    let mut accepted_seen = false;
    for (case, kind, location, expected) in plan {
        let (context, mut pairs) = fresh().await;
        if let Some(case) = case {
            pairs.retain(|(name, _)| !(name == &case.param && matches!(case.edit, Edit::Omit)));
            if let Edit::Set(raw) = &case.edit {
                pairs = with_param(&pairs, &case.param, raw);
            }
        }
        let probe = send(&context, encode(&pairs)).await;
        same_operation(&probe.captured, method, template)?;
        let handler = probe
            .verdict()
            .with_context(|| format!("{method} {template}: {kind} vector at {location}"))?;
        if handler != expected {
            return Err(mismatch(
                method, template, kind, &location, expected, &probe, handler,
            ));
        }
        accepted_seen |= handler;
        exchanges.push(probe.captured);
    }
    ensure!(accepted_seen, "no query vector was accepted by the handler");
    Ok(exchanges)
}

/// Query boundary vectors for one operation over real HTTP.
///
/// `fresh` yields, per request, a context and a VALID request (a login attempt
/// for a callback, for example); `seeds` give valid values for documented
/// optional parameters that request does not carry; `send` issues the request
/// target's query string and returns the exchange with the handler's verdict.
pub(crate) async fn query_vectors<C>(
    fixtures: &mut PositiveFixtures,
    method: &str,
    template: &str,
    seeds: &[(&str, &str)],
    fresh: impl AsyncFn() -> (C, Vec<(String, String)>),
    send: impl AsyncFn(&C, String) -> Probe,
) {
    query_vectors_extended(fixtures, method, template, seeds, &[], fresh, send).await;
}

/// [`query_vectors`] with `candidates`: `(parameter, value)` boundary values for a
/// parameter whose pattern has no derivable length bounds (the expected verdict
/// of each is computed from the schema; they must include accepted and refused ones).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn query_vectors_extended<C>(
    fixtures: &mut PositiveFixtures,
    method: &str,
    template: &str,
    seeds: &[(&str, &str)],
    candidates: &[(&str, &str)],
    fresh: impl AsyncFn() -> (C, Vec<(String, String)>),
    send: impl AsyncFn(&C, String) -> Probe,
) {
    let seeds = own(seeds);
    let candidates = own(candidates);
    let exchanges = run_query_with(
        responses::artifact(),
        method,
        template,
        &seeds,
        &candidates,
        &fresh,
        &send,
    )
    .await
    .unwrap_or_else(|e| responses::refuse(e));
    for exchange in &exchanges {
        fixtures.observe(exchange);
    }
    fixtures.record_vectors(method, template, "query");
}

fn own(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs
        .iter()
        .map(|(n, v)| ((*n).to_owned(), (*v).to_owned()))
        .collect()
}

/// [`query_vectors`] for operations whose valid request is a fixed query.
pub(crate) async fn query_vectors_fixed(
    fixtures: &mut PositiveFixtures,
    method: &str,
    template: &str,
    base: &[(&str, &str)],
    send: impl AsyncFn(String) -> Captured,
) {
    let base = own(base);
    query_vectors(
        fixtures,
        method,
        template,
        &[],
        async || ((), base.clone()),
        async |_: &(), query| Probe::from(send(query).await),
    )
    .await;
}

// ---------------------------------------------------------------------------
// Body vectors
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
enum Step {
    Key(String),
    Index(usize),
}
#[derive(Clone, Debug)]
pub(super) struct BodyCase {
    pub(super) pointer: String,
    pub(super) kind: &'static str,
    pub(super) body: Value,
    pub(super) accept: bool,
}

fn set(root: &Value, path: &[Step], new: Option<Value>) -> Value {
    let mut out = root.clone();
    let Some((last, parents)) = path.split_last() else {
        return new.unwrap_or(Value::Null);
    };
    let mut at = &mut out;
    for step in parents {
        at = match step {
            Step::Key(name) => &mut at[name.as_str()],
            Step::Index(i) => &mut at[*i],
        };
    }
    match (last, new) {
        (Step::Key(name), Some(v)) => {
            at[name.as_str()] = v;
        }
        (Step::Key(name), None) => {
            if let Some(map) = at.as_object_mut() {
                map.remove(name);
            }
        }
        (Step::Index(i), Some(v)) => {
            // One past the end appends (a seeded trailing positional item).
            if let Some(items) = at.as_array_mut()
                && *i == items.len()
            {
                items.push(v);
            } else {
                at[*i] = v;
            }
        }
        (Step::Index(i), None) => {
            if let Some(items) = at.as_array_mut()
                && *i < items.len()
            {
                items.remove(*i);
            }
        }
    }
    out
}
/// The JSON pointer of `path` (`""` for the root).
fn raw_pointer(path: &[Step]) -> String {
    path.iter()
        .map(|step| match step {
            Step::Key(name) => format!("/{name}"),
            Step::Index(i) => format!("/{i}"),
        })
        .collect()
}
fn pointer(path: &[Step]) -> String {
    if path.is_empty() {
        return "(root)".into();
    }
    path.iter()
        .map(|step| match step {
            Step::Key(name) => format!("/{name}"),
            Step::Index(i) => format!("/{i}"),
        })
        .collect()
}

struct Walk<'a> {
    document: &'a Value,
    root_schema: &'a Value,
    /// The valid body the current mutations are derived from (a seeded walk
    /// derives from the body with its seed added).
    valid_body: Value,
    /// The caller's valid body: never sent again as a vector.
    original: &'a Value,
    /// Caller-supplied valid values for declared optional locations, by pointer.
    seeds: std::collections::BTreeMap<String, Value>,
    used: BTreeSet<String>,
    /// Caller-supplied boundary candidates for locations whose pattern has no
    /// derivable bounds, by pointer; `candidates_used` are the locations walked.
    candidates: std::collections::BTreeMap<String, Vec<Value>>,
    candidates_used: BTreeSet<String>,
    seen: BTreeSet<String>,
    /// Bases already walked from their root (a seed is walked once per base).
    bases: BTreeSet<String>,
    /// Locations carrying [`UNSENDABLE`]: was the reason used by any walk?
    exempt_used: std::collections::BTreeMap<String, bool>,
    out: Vec<BodyCase>,
    notes: Notes,
    /// Every base body a walk started from (the valid body and each seeded
    /// base) with the instance locations (JSON pointers, `""` for the root) the
    /// walk generated vectors for: what `request_reach` may claim as reached.
    walked: std::collections::BTreeMap<String, (Value, BTreeSet<String>)>,
}
impl Walk<'_> {
    fn push(&mut self, path: &[Step], kind: &'static str, new: Option<Value>) -> Result<()> {
        let body = set(&self.valid_body, path, new);
        if body == self.valid_body
            || body == *self.original
            || !self.seen.insert(serde_json::to_string(&body)?)
        {
            return Ok(());
        }
        let accept = accepts(self.document, self.root_schema, &body)?;
        self.out.push(BodyCase {
            pointer: pointer(path),
            kind,
            body,
            accept,
        });
        Ok(())
    }
    /// The caller's seed for an absent declared location: send it ADDED to the
    /// valid body, then walk the WHOLE seeded body from its root (a seed such as
    /// `/mode` changes which conditional arms and union branches constrain the
    /// siblings, so the siblings must be mutated in the seeded base too).
    fn seeded(&mut self, path: &mut Vec<Step>) -> Result<()> {
        let at = pointer(path);
        let Some(seed) = self.seeds.get(&at).cloned() else {
            return Ok(());
        };
        self.used.insert(at);
        self.push(path, "optional added", Some(seed.clone()))?;
        let base = set(&self.valid_body, path, Some(seed));
        if self.bases.insert(serde_json::to_string(&base)?) {
            let saved = std::mem::replace(&mut self.valid_body, base.clone());
            let walked = self.walk(self.root_schema, &base, &mut Vec::new());
            self.valid_body = saved;
            walked?;
        }
        Ok(())
    }
    /// [`Notes::settle`] that also records whether a reviewed
    /// [`UNSENDABLE`] reason was actually USED at this location (a decisive
    /// vector really was unsendable): an unused one is stale.
    fn settle_at(&mut self, here: &str, dropped: &[String], sent: &[&str], exempt: bool) {
        let before = self.notes.exempt;
        self.notes.settle(here, dropped, sent, exempt);
        if exempt {
            let used = self.exempt_used.entry(here.to_owned()).or_insert(false);
            *used |= self.notes.exempt > before;
        }
    }

    /// The array a vector sends with `length` elements: the valid array's own
    /// elements first, then per-position values (a seeded positional value) or,
    /// for the homogeneous tail, copies of a valid tail element (DISTINCT ones
    /// under `uniqueItems`). `None` when no valid element can be built for a
    /// position.
    fn array_of(
        &self,
        nodes: &[&Value],
        items: &[Value],
        path: &[Step],
        length: usize,
        unique: bool,
    ) -> Result<Option<Vec<Value>>> {
        let mut out: Vec<Value> = items.iter().take(length).cloned().collect();
        let start = out.len();
        let tail_from = tail_start(nodes);
        let template = items.get(tail_from).cloned();
        let wanted = |i: usize| !positional(nodes, i) && element_schema(nodes, i).is_some();
        let fresh = (start..length).filter(|i| wanted(*i)).count();
        let mut variants = Vec::new();
        if unique && fresh > 0 {
            let (Some(template), Some(schema)) = (&template, element_schema(nodes, tail_from))
            else {
                return Ok(None);
            };
            let Some(found) = distinct_variants(self.document, &schema, template, items, fresh)?
            else {
                return Ok(None);
            };
            variants = found;
        }
        for i in start..length {
            if positional(nodes, i) {
                let mut at = path.to_vec();
                at.push(Step::Index(i));
                let Some(seed) = self.seeds.get(&pointer(&at)) else {
                    return Ok(None);
                };
                out.push(seed.clone());
            } else if element_schema(nodes, i).is_some() {
                match (unique, &template) {
                    (true, _) => out.extend(variants.pop()),
                    (false, Some(template)) => out.push(template.clone()),
                    (false, None) => return Ok(None),
                }
            } else {
                // No schema governs this position (a closed tuple): any element
                // is refused structurally; repeat the last one.
                let Some(last) = out.last().cloned() else {
                    return Ok(None);
                };
                out.push(last);
            }
        }
        Ok(Some(out))
    }

    /// `minItems`/`maxItems` vectors, one meaningful pair per distinct bound.
    fn array_bounds(
        &mut self,
        schema: &Value,
        nodes: &[&Value],
        items: &[Value],
        path: &mut Vec<Step>,
    ) -> Result<()> {
        let mins = distinct_u64(nodes, "minItems");
        let maxs = distinct_u64(nodes, "maxItems");
        let unique = nodes
            .iter()
            .any(|node| node.get("uniqueItems") == Some(&Value::Bool(true)));
        let described = nodes
            .iter()
            .any(|node| node.get("items").is_some() || node.get("prefixItems").is_some());
        let mut evidence = Evidence::default();
        let mut plan: Vec<(&'static str, &'static str, u64, Option<usize>, usize)> = Vec::new();
        for &min in mins.iter().filter(|min| **min > 0) {
            plan.push((
                "minItems",
                "minItems-1",
                min,
                Some(min as usize),
                min as usize - 1,
            ));
        }
        for &max in &maxs {
            plan.push((
                "maxItems",
                "maxItems+1",
                max,
                Some(max as usize),
                max as usize + 1,
            ));
        }
        for (at_kind, over_kind, bound, at, over) in plan {
            let key = if at_kind == "minItems" {
                "minItems"
            } else {
                "maxItems"
            };
            let dominated = if key == "minItems" {
                Some(&bound) != mins.last()
            } else {
                Some(&bound) != maxs.first()
            };
            let kinds = (at_kind, Some(over_kind));
            let (Some(at), over) = (at, over) else {
                continue;
            };
            // An array whose elements no schema describes cannot be filled with
            // valid elements: nothing says what a valid one is.
            if !described || at.max(over) as u64 > MAX_ITEMS {
                evidence.settle(kinds, bound, None, dominated);
                continue;
            }
            let built = (
                self.array_of(nodes, items, path, at, unique)?,
                self.array_of(nodes, items, path, over, unique)?,
            );
            let (Some(at), Some(over)) = built else {
                evidence.settle(kinds, bound, None, dominated);
                continue;
            };
            let validator = relaxed(self.document, schema, key, bound)?;
            let found = find_pair(
                self.document,
                schema,
                Some(&validator),
                std::iter::once((Value::Array(at.clone()), Value::Array(over.clone()))),
            )?;
            if found.is_none() {
                // Still sent: the handler must agree with the schema on them.
                self.push(path, at_kind, Some(Value::Array(at)))?;
                self.push(path, over_kind, Some(Value::Array(over)))?;
            }
            evidence.settle(kinds, bound, found, dominated);
        }
        let sent: Vec<&str> = evidence
            .labelled
            .iter()
            .map(|(label, ..)| label.as_str())
            .collect();
        let exempt = nodes.iter().any(|node| node.get(UNSENDABLE).is_some());
        let here = pointer(path);
        self.settle_at(&here, &evidence.dropped, &sent, exempt);
        for (_, kind, value) in evidence.labelled {
            self.push(path, kind, Some(value))?;
        }
        Ok(())
    }
    fn walk(&mut self, schema: &Value, value: &Value, path: &mut Vec<Step>) -> Result<()> {
        ensure!(path.len() < 12, "request body nesting too deep");
        self.walked
            .entry(serde_json::to_string(&self.valid_body)?)
            .or_insert_with(|| (self.valid_body.clone(), BTreeSet::new()))
            .1
            .insert(raw_pointer(path));
        let mut nodes = Vec::new();
        applicable(self.document, schema, value, &mut nodes, 0)?;
        let here = pointer(path);
        let supplied = self.candidates.get(&here).cloned().unwrap_or_default();
        let mutations =
            scalar_mutations(self.document, schema, &nodes, value, false, supplied.len())?;
        if mutations.needs_candidates {
            self.notes.problems.push(format!(
                "{here}: the pattern has no derivable length bounds; supply boundary candidates or annotate it with {PATTERN_BOUNDARIES}"
            ));
        }
        if mutations.stale_pattern_boundaries {
            self.notes.problems.push(format!(
                "{here}: {PATTERN_BOUNDARIES} is stale: the pattern, a maxLength or the supplied candidates already cover its boundaries; delete it"
            ));
        }
        let sent: Vec<&str> = mutations
            .vectors
            .iter()
            .map(|(kind, _)| *kind)
            .chain(mutations.labelled.iter().map(|(label, ..)| label.as_str()))
            .collect();
        let exempt = nodes.iter().any(|node| node.get(UNSENDABLE).is_some());
        if exempt {
            self.exempt_used.entry(here.clone()).or_insert(false);
        }
        self.settle_at(&here, &mutations.dropped, &sent, exempt);
        // Decisive kinds first: a body sent twice keeps the first kind's name.
        for (_, kind, new) in mutations.labelled {
            self.push(path, kind, Some(new))?;
        }
        for (kind, new) in mutations.vectors {
            self.push(path, kind, Some(new))?;
        }
        if !supplied.is_empty() {
            self.candidates_used.insert(here.clone());
            let verdicts = supplied
                .iter()
                .map(|candidate| {
                    Ok((
                        render(candidate),
                        accepts(self.document, schema, candidate)?,
                    ))
                })
                .collect::<Result<Vec<_>>>()?;
            if !straddles(&verdicts) {
                self.notes.problems.push(format!("{here}: {STRADDLE}"));
            }
            for candidate in supplied {
                self.push(path, "supplied candidate", Some(candidate))?;
            }
        }
        if !path.is_empty() {
            self.push(path, "omitted", None)?;
        }
        match value {
            Value::Object(map) => {
                let mut unknown = map.clone();
                unknown.insert("unexpected_configuration".into(), json!(true));
                self.push(path, "unknown field", Some(Value::Object(unknown)))?;
                for (name, child) in map {
                    // Only schema-declared names (properties and typed map
                    // values) are walked (and reported).
                    if let Some(child_schema) = member_schema(&nodes, name)? {
                        path.push(Step::Key(name.clone()));
                        self.walk(&child_schema, child, path)?;
                        path.pop();
                    }
                }
                // Declared optional properties the valid body omits: only the
                // caller's seeds make them reachable.
                let absent: BTreeSet<&String> = nodes
                    .iter()
                    .filter_map(|node| node["properties"].as_object())
                    .flat_map(|properties| properties.keys())
                    .filter(|name| !map.contains_key(*name))
                    .collect();
                for name in absent {
                    if child_schema(&nodes, name).is_some() {
                        path.push(Step::Key(name.clone()));
                        self.seeded(path)?;
                        path.pop();
                    }
                }
            }
            Value::Array(items) => {
                self.array_bounds(schema, &nodes, items, path)?;
                self.push(path, "empty array", Some(json!([])))?;
                // Every positional index, and one element of each distinct
                // signature (the union arms and conditional branches it matches)
                // of the homogeneous `items` tail: a second element that takes
                // another arm is a different location and must be mutated too.
                let mut signatures = BTreeSet::new();
                for (i, item) in items.iter().enumerate() {
                    let Some(item_schema) = element_schema(&nodes, i) else {
                        continue;
                    };
                    if !positional(&nodes, i) {
                        let signature = request_reach::reached(self.document, &item_schema, item)?;
                        if !signatures.insert(signature) {
                            continue;
                        }
                    }
                    path.push(Step::Index(i));
                    self.walk(&item_schema, item, path)?;
                    path.pop();
                }
                // The next positional item, when the caller seeded it.
                if positional(&nodes, items.len()) {
                    path.push(Step::Index(items.len()));
                    self.seeded(path)?;
                    path.pop();
                }
            }
            _ => {}
        }
        Ok(())
    }
}
/// All schemas a property or item must satisfy, as one `allOf` when several
/// (the base object and the matching union/conditional arms each constrain it).
fn combine(mut parts: Vec<Value>) -> Option<Value> {
    match parts.len() {
        0 => None,
        1 => parts.pop(),
        _ => Some(json!({"allOf": parts})),
    }
}
fn child_schema(nodes: &[&Value], name: &str) -> Option<Value> {
    combine(
        nodes
            .iter()
            .filter_map(|node| node["properties"].get(name))
            .cloned()
            .collect(),
    )
}
/// The schema of an object member: its `properties` entry plus every
/// `patternProperties` value whose pattern matches the name, or the schema-valued
/// `additionalProperties` when neither selects it.
fn member_schema(nodes: &[&Value], name: &str) -> Result<Option<Value>> {
    let mut parts: Vec<Value> = nodes
        .iter()
        .filter_map(|node| node["properties"].get(name))
        .cloned()
        .collect();
    let mut selected = !parts.is_empty();
    for node in nodes {
        for (pattern, child) in node["patternProperties"].as_object().into_iter().flatten() {
            if child.is_object()
                && super::validation::compile_schema(&json!({"type":"string","pattern":pattern}))?
                    .is_valid(&Value::String(name.to_owned()))
            {
                selected = true;
                parts.push(child.clone());
            }
        }
    }
    if !selected {
        parts.extend(
            nodes
                .iter()
                .filter_map(|node| node.get("additionalProperties").filter(|v| v.is_object()))
                .cloned(),
        );
    }
    Ok(combine(parts))
}
/// Does any applicable node declare a positional (`prefixItems`) schema at `index`?
fn positional(nodes: &[&Value], index: usize) -> bool {
    nodes
        .iter()
        .any(|node| node["prefixItems"].get(index).is_some())
}
/// `need` NEW distinct values the `schema` accepts, none equal to `taken`,
/// found by varying `template` (characters, numbers, flags, member values).
/// `None` when the schema has too few distinct values (or the search gives up).
fn distinct_variants(
    document: &Value,
    schema: &Value,
    template: &Value,
    taken: &[Value],
    need: usize,
) -> Result<Option<Vec<Value>>> {
    const ALPHABET: &str = "0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ";
    fn mutate(value: &Value) -> Vec<Value> {
        match value {
            Value::String(text) => {
                let chars: Vec<char> = text.chars().collect();
                let mut out = Vec::new();
                for position in 0..chars.len().min(16) {
                    for c in ALPHABET.chars().filter(|c| *c != chars[position]) {
                        let mut changed = chars.clone();
                        changed[position] = c;
                        out.push(json!(changed.into_iter().collect::<String>()));
                    }
                }
                for c in ALPHABET.chars() {
                    out.push(json!(format!("{text}{c}")));
                }
                out
            }
            Value::Number(n) => {
                let n = n.as_i64().unwrap_or(0);
                (1..=8)
                    .flat_map(|k| [json!(n.saturating_add(k)), json!(n.saturating_sub(k))])
                    .collect()
            }
            Value::Bool(flag) => vec![json!(!flag)],
            Value::Object(map) => map
                .iter()
                .flat_map(|(name, child)| {
                    mutate(child).into_iter().map(move |changed| {
                        let mut copy = map.clone();
                        copy.insert(name.clone(), changed);
                        Value::Object(copy)
                    })
                })
                .collect(),
            Value::Array(items) => items
                .iter()
                .enumerate()
                .flat_map(|(i, child)| {
                    mutate(child).into_iter().map(move |changed| {
                        let mut copy = items.clone();
                        copy[i] = changed;
                        Value::Array(copy)
                    })
                })
                .collect(),
            Value::Null => Vec::new(),
        }
    }
    let validator = responses::validator(document, schema)?;
    let mut seen: BTreeSet<String> = taken
        .iter()
        .chain([template])
        .map(serde_json::to_string)
        .collect::<Result<_, _>>()?;
    let mut queue: std::collections::VecDeque<Value> = taken.iter().cloned().collect();
    queue.push_back(template.clone());
    let mut found = Vec::new();
    let mut budget = 50_000usize;
    while found.len() < need
        && budget > 0
        && let Some(base) = queue.pop_front()
    {
        for candidate in mutate(&base) {
            budget = budget.saturating_sub(1);
            if found.len() >= need || !seen.insert(serde_json::to_string(&candidate)?) {
                continue;
            }
            if validator.is_valid(&candidate) {
                found.push(candidate.clone());
                queue.push_back(candidate);
            }
        }
    }
    Ok((found.len() >= need).then_some(found))
}
/// The schema every element at `index` must satisfy: for EACH applicable node,
/// its positional schema at `index` when it has one, else its homogeneous
/// `items` (a sibling `allOf` member's `items` governs a position another
/// member's `prefixItems` also covers).
fn element_schema(nodes: &[&Value], index: usize) -> Option<Value> {
    combine(
        nodes
            .iter()
            .filter_map(
                |node| match node["prefixItems"].as_array().and_then(|p| p.get(index)) {
                    Some(positional) => Some(positional.clone()),
                    None => node.get("items").filter(|v| v.is_object()).cloned(),
                },
            )
            .collect(),
    )
}
/// The first index `items` governs: after every positional schema.
fn tail_start(nodes: &[&Value]) -> usize {
    nodes
        .iter()
        .filter_map(|node| node["prefixItems"].as_array().map(Vec::len))
        .max()
        .unwrap_or(0)
}

fn body_schema(operation: &Value) -> Result<&Value> {
    operation["requestBody"]["content"]["application/json"]
        .get("schema")
        .context("the operation documents no JSON request body")
}

pub(super) struct BodyPlan {
    pub(super) cases: Vec<BodyCase>,
    pub(super) notes: Notes,
    /// The declared request locations (schema paths) the valid body and the
    /// seeded bases carry; see `request_reach`.
    pub(super) reached: BTreeSet<String>,
}

/// Every body vector derived from the operation's request schema and a VALID body.
pub(super) fn body_cases(
    document: &Value,
    operation: &Value,
    valid_body: &Value,
    seeds: &[(String, Value)],
) -> Result<Vec<BodyCase>> {
    Ok(body_plan(document, operation, valid_body, seeds, &[])?.cases)
}

/// [`body_cases`] with the account of what could not be sent. Fails when a
/// decisive boundary vector cannot be sent (see [`UNSENDABLE`]).
pub(super) fn body_plan(
    document: &Value,
    operation: &Value,
    valid_body: &Value,
    seeds: &[(String, Value)],
    candidates: &[(String, Value)],
) -> Result<BodyPlan> {
    let schema = body_schema(operation)?;
    ensure!(
        accepts(document, schema, valid_body)?,
        "the valid request body must satisfy its schema"
    );
    let mut walk = Walk {
        document,
        root_schema: schema,
        valid_body: valid_body.clone(),
        original: valid_body,
        seeds: seeds.iter().cloned().collect(),
        used: BTreeSet::new(),
        candidates: candidates.iter().fold(
            std::collections::BTreeMap::new(),
            |mut all, (at, value)| {
                all.entry(at.clone())
                    .or_insert_with(Vec::new)
                    .push(value.clone());
                all
            },
        ),
        candidates_used: BTreeSet::new(),
        seen: BTreeSet::new(),
        bases: BTreeSet::from([serde_json::to_string(valid_body)?]),
        exempt_used: std::collections::BTreeMap::new(),
        out: Vec::new(),
        notes: Notes::default(),
        walked: std::collections::BTreeMap::new(),
    };
    walk.walk(schema, valid_body, &mut Vec::new())?;
    for (at, _) in seeds {
        ensure!(
            walk.used.contains(at),
            "seed at {at} names no declared optional location that the valid body omits"
        );
    }
    for (at, _) in candidates {
        ensure!(
            walk.candidates_used.contains(at),
            "candidate at {at} names no walked location"
        );
    }
    for (location, used) in &walk.exempt_used {
        if !used {
            walk.notes.problems.push(format!(
                "{location}: {UNSENDABLE} is stale: every decisive vector of this location can be sent; delete it"
            ));
        }
    }
    walk.notes.check()?;
    // Reach is what the walk MUTATED, base by base: a location the body merely
    // contains (but the walk never generated a vector for) is not reached.
    let mut reached = BTreeSet::new();
    for (base, paths) in walk.walked.values() {
        reached.extend(request_reach::reached_by_walk(
            document, schema, base, paths,
        )?);
    }
    Ok(BodyPlan {
        cases: walk.out,
        notes: walk.notes,
        reached,
    })
}

/// One executed body vector run: the exchanges and the declared request
/// locations (schema paths) its valid body and seeded bases carried.
pub(super) struct BodyRun {
    pub(super) exchanges: Vec<Captured>,
    pub(super) reached: BTreeSet<String>,
}

pub(super) async fn run_body(
    document: &Value,
    method: &str,
    template: &str,
    valid_body: Value,
    seeds: &[(String, Value)],
    send: &impl AsyncFn(Value) -> Probe,
) -> Result<Vec<Captured>> {
    Ok(
        run_body_with(document, method, template, valid_body, seeds, &[], send)
            .await?
            .exchanges,
    )
}

pub(super) async fn run_body_with(
    document: &Value,
    method: &str,
    template: &str,
    valid_body: Value,
    seeds: &[(String, Value)],
    candidates: &[(String, Value)],
    send: &impl AsyncFn(Value) -> Probe,
) -> Result<BodyRun> {
    let operation = responses::operation(document, method, template)?;
    let plan = body_plan(document, operation, &valid_body, seeds, candidates)
        .with_context(|| format!("{method} {template}"))?;
    plan.notes.report(method, template);
    let reached = plan.reached;
    let cases = plan.cases;
    let mut plan = vec![("valid", "(root)".to_owned(), valid_body.clone(), true)];
    plan.extend(
        cases
            .into_iter()
            .map(|case| (case.kind, case.pointer, case.body, case.accept)),
    );
    let mut exchanges = Vec::new();
    let mut accepted_seen = false;
    for (kind, location, body, expected) in plan {
        let probe = send(body).await;
        same_operation(&probe.captured, method, template)?;
        let handler = probe
            .verdict()
            .with_context(|| format!("{method} {template}: {kind} vector at {location}"))?;
        if handler != expected {
            return Err(mismatch(
                method, template, kind, &location, expected, &probe, handler,
            ));
        }
        accepted_seen |= handler;
        exchanges.push(probe.captured);
    }
    ensure!(accepted_seen, "no body vector was accepted by the handler");
    Ok(BodyRun { exchanges, reached })
}

/// Body boundary vectors for one operation over real HTTP: top-level and nested
/// wrong types, null where not nullable, enum members and non-members, bounds,
/// flipped booleans and consts, pattern and length boundaries, every positional
/// index, omissions and unknown fields. `valid` must satisfy the schema; supply
/// optional nested fields in it to have them walked too, or use
/// [`body_vectors_seeded`] for optional siblings that cross-field rules relate.
pub(crate) async fn body_vectors_http(
    fixtures: &mut PositiveFixtures,
    method: &str,
    template: &str,
    valid: Value,
    send: impl AsyncFn(Value) -> Probe,
) {
    body_vectors_seeded(fixtures, method, template, valid, &[], send).await;
}

/// [`body_vectors_http`] with caller-supplied `seeds`: `(JSON pointer, valid
/// value)` for declared optional properties (or the next positional item) that
/// `valid` omits. Each seed is sent ADDED to `valid` (so `{"ids":[x]}` plus
/// `/all: true` reaches the handler) and its value is walked like any other.
/// A seed that names nothing the body omits fails the run.
pub(crate) async fn body_vectors_seeded(
    fixtures: &mut PositiveFixtures,
    method: &str,
    template: &str,
    valid: Value,
    seeds: &[(&str, Value)],
    send: impl AsyncFn(Value) -> Probe,
) {
    body_vectors_extended(fixtures, method, template, valid, seeds, &[], send).await;
}

/// [`body_vectors_seeded`] with `candidates`: `(JSON pointer, value)` boundary
/// values the fixture supplies for a string whose pattern has no derivable length
/// bounds (for example `^(0|[1-9][0-9]{0,14})$`: 15 and 16 digits, and "0"). Each is
/// sent in place of the value at its pointer and judged by the schema; they must
/// include a value the schema accepts and one it refuses, and the pointer must be a
/// walked location.
pub(crate) async fn body_vectors_extended(
    fixtures: &mut PositiveFixtures,
    method: &str,
    template: &str,
    valid: Value,
    seeds: &[(&str, Value)],
    candidates: &[(&str, Value)],
    send: impl AsyncFn(Value) -> Probe,
) {
    let own_values = |pairs: &[(&str, Value)]| -> Vec<(String, Value)> {
        pairs
            .iter()
            .map(|(pointer, value)| ((*pointer).to_owned(), value.clone()))
            .collect()
    };
    let run = run_body_with(
        responses::artifact(),
        method,
        template,
        valid,
        &own_values(seeds),
        &own_values(candidates),
        &send,
    )
    .await
    .unwrap_or_else(|e| responses::refuse(e));
    for exchange in &run.exchanges {
        fixtures.observe(exchange);
    }
    fixtures.record_vectors(method, template, "body");
    fixtures.record_reach(method, template, &run.reached);
}
