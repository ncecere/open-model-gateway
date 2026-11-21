//! Never log plaintext credentials, inference tokens or prompt/response bodies
//! (AGENTS.md invariant, #591). In the request-handling code, every value a
//! logging call can carry is pinned to a reviewed snapshot, so a new logged
//! field shows up in review and cannot arrive by accident.
//!
//! Every production file is scanned. For every `tracing` event or span macro
//! (`trace!`..`error!`, `event!`, `span!`, `*_span!`, as `tracing::x!`,
//! `log::x!` or bare, also inside the body of any other macro), `#[instrument]`
//! attribute (also inside `cfg_attr`), `print!`-family macro, and call of a
//! `macro_rules!` whose body forwards to one of those (a wrapper: its call
//! sites are log calls), the test records the macro, its function, each
//! *field* (`name = value` with its `%`/`?` sigil, or a shorthand `name`), and
//! each *format argument* (inline `{name}` captures and positional or named
//! arguments), as whitespace-free token text.
//!
//! * In the request path ([`SCOPE`]: `inference/`, `providers/`, `protocols/`,
//!   `jobs/`, `http.rs`, `http/`, `auth.rs`, `auth/`) the sorted list must
//!   equal `log_fields.snapshot` line for line. Adding, removing or changing a
//!   logged value is a diff of that file.
//! * In every file, with no snapshot entry needed to fail: an identifier or
//!   field name made of a word from [`DENIED_WORDS`] (`body`, `payload`,
//!   `prompt`, `response`, `request`, `headers`, `content`, `messages`,
//!   `token`, `jwt`, `session`, `secret`, `creds`, `credential`, `password`,
//!   `authorization`, `bearer`, `cookie`, ...) or `api` + `key`, in a logged
//!   name or value (string literals in values do not count), is refused
//!   unless the exact (entry, word) is in [`EXEMPT`] with its reason; a
//!   stale exemption fails. And `#[instrument]` without `skip_all` (it
//!   records every argument with `Debug`) and `ret` (it records the return
//!   value) are refused.
//!
//! Cannot catch: a value formatted into an allowed field (`error = ?e` is a
//! `Debug` of whatever `e` holds, and `%path` of a path that embeds a secret);
//! a `Debug`/`Display` impl that prints a secret field (see the redaction
//! tests of the types themselves); a logging macro imported under another
//! name (`use tracing::warn as w;`); `panic!`, `assert!`, `unwrap`/`expect`
//! messages, `anyhow` context and error `Display` strings that reach a log
//! through an error value; direct writes to stderr or stdout
//! (`io::stderr().write_all`, `writeln!`); logging through `ops_log` events,
//! writers or the metrics labels (each has its own tests); `Span::record`
//! calls; a wrapper macro defined as a `macro_rules!` with a procedural
//! macro in between.
use std::collections::BTreeSet;

use proc_macro2::{Delimiter, TokenStream, TokenTree};
use syn::{ext::IdentExt, visit::Visit};

use super::tree::{Scope, Source};

/// Request-handling code: where prompts, responses and credentials flow.
const SCOPE: &[&str] = &[
    "inference/",
    "providers/",
    "protocols/",
    "jobs/",
    "http.rs",
    "http/",
    "auth.rs",
    "auth/",
];

const SNAPSHOT: &str = include_str!("log_fields.snapshot");

/// Words that make a logged name or value a credential or a body.
const DENIED_WORDS: &[&str] = &[
    "body",
    "bodies",
    "prompt",
    "prompts",
    "completion",
    "completions",
    "messages",
    "content",
    "contents",
    "input",
    "secret",
    "secrets",
    "credential",
    "credentials",
    "password",
    "passwords",
    "authorization",
    "bearer",
    "cookie",
    "cookies",
    "plaintext",
    "token",
    "apikey",
    "payload",
    "payloads",
    "response",
    "responses",
    "request",
    "requests",
    "headers",
    "jwt",
    "creds",
    "session",
    "sessions",
];

/// (exact entry line, word): reviewed uses of a denied word, with the
/// reason. A stale entry (no such line, or the word no longer in it) fails.
///
/// * `request_id` and `execution_id=%request_id` are the UUID the gateway
///   generated for the request (`http.rs`: "do not reflect arbitrary
///   client-supplied values into logs"), never request content;
/// * `request.method()` is the HTTP method; `response.status()` the status
///   code: both are fixed, content-free values;
/// * `commands.rs` `bootstrap_dev` prints the development keys to stdout
///   once, on purpose ("Development keys: save these now. They cannot be
///   retrieved again."). It is the operator's terminal output of a local
///   command, not a log line; the keys are fresh credentials for a local
///   development database. That "local" basis rests on `GATEWAY_ENV`, which
///   is tracked in #632.
const EXEMPT: &[(&str, &str)] = &[
    (
        "http.rs :: request_context :: info :: header_latency_ms=started.elapsed().as_millis() ; status=response.status().as_u16()",
        "response",
    ),
    (
        "http.rs :: request_context :: info_span :: method=%request.method() ; request_id=request_id",
        "request",
    ),
    (
        "jobs/video.rs :: impl Jobs::create_video :: error :: execution_id=%request_id",
        "request",
    ),
    (
        "commands.rs :: bootstrap_dev :: println :: arg serde_json::to_string_pretty(&serde_json::json!({\"notice\":\"Developmentkeys:savethesenow.Theycannotberetrievedagain.\",\"installation_id\":keys.installation_id,\"personal_workspace_id\":keys.personal_workspace_id,\"team_workspace_id\":keys.team_workspace_id,\"personal_api_key\":keys.personal_key.token,\"team_api_key\":keys.team_key.token,}))?",
        "token",
    ),
];

const EVENTS: &[&str] = &["trace", "debug", "info", "warn", "error", "event"];
const SPANS: &[&str] = &[
    "span",
    "trace_span",
    "debug_span",
    "info_span",
    "warn_span",
    "error_span",
];
const PRINTS: &[&str] = &["print", "println", "eprint", "eprintln", "dbg"];

/// The text with the contents of double-quoted strings removed.
fn without_literals(text: &str) -> String {
    let mut out = String::new();
    let mut inside = false;
    let mut escaped = false;
    for c in text.chars() {
        if inside {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                inside = false;
            }
        } else if c == '"' {
            inside = true;
            out.push(' ');
        } else {
            out.push(c);
        }
    }
    out
}

/// Denied words in an entry item: a field name, a value, or a format
/// argument. Fixed string text in a value or argument does not count.
fn denied_item(item: &str) -> BTreeSet<String> {
    match item.split_once('=') {
        Some((name, value)) if !item.starts_with("arg ") => {
            let mut out = denied(name);
            out.extend(denied(&without_literals(value)));
            out
        }
        _ => denied(&without_literals(item)),
    }
}

/// Denied words in a stretch of token text (a name or an expression).
fn denied(text: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for words in super::tree::word_forms(text) {
        if words
            .windows(2)
            .any(|pair| pair[0] == "api" && pair[1] == "key")
        {
            out.insert("api_key".to_owned());
        }
        out.extend(
            words
                .into_iter()
                .filter(|word| DENIED_WORDS.contains(&word.as_str())),
        );
    }
    out
}

fn compact(tokens: impl IntoIterator<Item = TokenTree>) -> String {
    let stream: TokenStream = tokens.into_iter().collect();
    stream
        .to_string()
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect()
}

fn is_punct(token: Option<&TokenTree>, c: char) -> bool {
    matches!(token, Some(TokenTree::Punct(p)) if p.as_char() == c)
}

fn string_literal(token: &TokenTree) -> Option<String> {
    match token {
        TokenTree::Literal(l) => match syn::parse_str::<syn::Lit>(&l.to_string()) {
            Ok(syn::Lit::Str(s)) => Some(s.value()),
            _ => None,
        },
        _ => None,
    }
}

/// Top-level comma-separated segments of a macro body.
fn segments(tokens: TokenStream) -> Vec<Vec<TokenTree>> {
    let mut out = vec![Vec::new()];
    for token in tokens {
        if is_punct(Some(&token), ',') {
            out.push(Vec::new());
        } else if let Some(last) = out.last_mut() {
            last.push(token);
        }
    }
    if out.last().is_some_and(Vec::is_empty) {
        out.pop();
    }
    out
}

/// `{name}` and `{name:?}` captures of a format string.
fn captures(format: &str) -> Vec<String> {
    let mut out = Vec::new();
    let chars: Vec<char> = format.chars().collect();
    let mut at = 0;
    while at < chars.len() {
        if chars[at] == '{' {
            if chars.get(at + 1) == Some(&'{') {
                at += 2;
                continue;
            }
            let mut end = at + 1;
            while end < chars.len() && chars[end] != '}' && chars[end] != ':' {
                end += 1;
            }
            let name: String = chars[at + 1..end].iter().collect();
            let name = name.trim().to_owned();
            if name
                .chars()
                .next()
                .is_some_and(|c| c.is_alphabetic() || c == '_')
            {
                out.push(name);
            }
            at = end;
        }
        at += 1;
    }
    out
}

/// One logging call, reduced to what it carries.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Clone)]
pub(super) struct Entry {
    file: String,
    scope: String,
    kind: String,
    /// Sorted `name=sigil value` and `fmt:...` items.
    items: Vec<String>,
}

impl Entry {
    pub(super) fn line(&self) -> String {
        format!(
            "{} :: {} :: {} :: {}",
            self.file,
            self.scope,
            self.kind,
            if self.items.is_empty() {
                "-".to_owned()
            } else {
                self.items.join(" ; ")
            }
        )
    }
}

/// Fields and arguments of one macro call.
fn read_call(name: &str, tokens: TokenStream) -> Vec<String> {
    let mut parts = segments(tokens);
    // `span!(Level::X, ..)` and `event!(Level::X, ..)`.
    if (name == "span" || name == "event") && !parts.is_empty() {
        parts.remove(0);
    }
    read_parts(SPANS.contains(&name), parts)
}

fn read_parts(span_like: bool, parts: Vec<Vec<TokenTree>>) -> Vec<String> {
    let mut items = Vec::new();
    let mut formatting = false;
    for part in parts {
        let mut tokens: &[TokenTree] = &part;
        // `target: ..` and `parent: ..`.
        if matches!(tokens.first(), Some(TokenTree::Ident(i)) if i == "target" || i == "parent")
            && is_punct(tokens.get(1), ':')
            && !is_punct(tokens.get(2), ':')
        {
            continue;
        }
        if tokens.is_empty() {
            continue;
        }
        if formatting {
            items.push(format!("arg {}", compact(tokens.iter().cloned())));
            continue;
        }
        // A bare string literal: the message (events) or the span name.
        if let Some(text) = string_literal(&tokens[0])
            && tokens.len() == 1
        {
            for capture in captures(&text) {
                items.push(format!("fmt {capture}"));
            }
            formatting = !span_like;
            continue;
        }
        // Field: [sigil] name [= [sigil] value].
        let mut sigil = "";
        if is_punct(tokens.first(), '%') {
            sigil = "%";
            tokens = &tokens[1..];
        } else if is_punct(tokens.first(), '?') {
            sigil = "?";
            tokens = &tokens[1..];
        }
        // The first `=` is the assignment: a field name never contains one.
        let eq = tokens
            .iter()
            .position(|t| matches!(t, TokenTree::Punct(p) if p.as_char() == '='));
        match eq {
            Some(at) => {
                let field = compact(tokens[..at].iter().cloned());
                let value = &tokens[at + 1..];
                let (inner, vsigil) = if is_punct(value.first(), '%') {
                    (&value[1..], "%")
                } else if is_punct(value.first(), '?') {
                    (&value[1..], "?")
                } else {
                    (value, "")
                };
                items.push(format!(
                    "{field}={sigil}{vsigil}{}",
                    compact(inner.iter().cloned())
                ));
            }
            None => {
                let shorthand = compact(tokens.iter().cloned());
                items.push(format!("{shorthand}={sigil}{shorthand}"));
            }
        }
    }
    items.sort();
    items
}

struct Finder<'a> {
    file: String,
    scope: Scope,
    wrappers: &'a BTreeSet<String>,
    entries: Vec<Entry>,
    problems: Vec<String>,
}

impl Finder<'_> {
    fn push(&mut self, kind: String, items: Vec<String>) {
        self.entries.push(Entry {
            file: self.file.clone(),
            scope: self.scope.path(),
            kind,
            items,
        });
    }

    /// Looks for logging macros anywhere in a macro body.
    fn tokens(&mut self, tokens: TokenStream) {
        let all: Vec<TokenTree> = tokens.into_iter().collect();
        for (at, token) in all.iter().enumerate() {
            match token {
                TokenTree::Ident(ident)
                    if is_punct(all.get(at + 1), '!')
                        && let Some(TokenTree::Group(group)) = all.get(at + 2) =>
                {
                    self.call(&ident.unraw().to_string(), group.stream());
                }
                TokenTree::Group(group) => self.tokens(group.stream()),
                _ => {}
            }
        }
    }

    fn call(&mut self, name: &str, tokens: TokenStream) {
        if EVENTS.contains(&name) || SPANS.contains(&name) || PRINTS.contains(&name) {
            let items = read_call(name, tokens.clone());
            self.push(name.to_owned(), items);
        } else if self.wrappers.contains(name) {
            // The call sites of a macro that forwards to a logging macro are
            // log calls: every argument is a logged value.
            let mut items: Vec<String> = segments(tokens.clone())
                .iter()
                .map(|part| format!("arg {}", compact(part.iter().cloned())))
                .collect();
            items.sort();
            self.push(format!("wrapper {name}"), items);
        }
        // A logging call can sit inside the arguments of another macro.
        self.tokens(tokens);
    }
}

impl<'ast> Visit<'ast> for Finder<'_> {
    track_scope!();

    fn visit_macro(&mut self, mac: &'ast syn::Macro) {
        let name = mac
            .path
            .segments
            .last()
            .map(|s| s.ident.unraw().to_string())
            .unwrap_or_default();
        self.call(&name, mac.tokens.clone());
    }

    fn visit_attribute(&mut self, attr: &'ast syn::Attribute) {
        let Some(last) = attr.path().segments.last() else {
            return;
        };
        let syn::Meta::List(list) = &attr.meta else {
            if last.ident == "instrument" {
                self.instrument(TokenStream::new());
            }
            return;
        };
        if last.ident == "instrument" {
            self.instrument(list.tokens.clone());
        } else if last.ident == "cfg_attr" {
            // `cfg_attr(<predicate>, instrument(..), ..)`: read like `instrument`.
            for part in segments(list.tokens.clone()).iter().skip(1) {
                if let Some(args) = instrument_part(part) {
                    self.instrument(args);
                }
            }
        }
    }
}

/// `instrument`, `tracing::instrument` or either with `(..)`: its arguments.
fn instrument_part(part: &[TokenTree]) -> Option<TokenStream> {
    let mut chain = Vec::new();
    let mut args = TokenStream::new();
    for token in part {
        match token {
            TokenTree::Ident(i) => chain.push(i.unraw().to_string()),
            TokenTree::Punct(p) if p.as_char() == ':' => {}
            TokenTree::Group(g) if g.delimiter() == Delimiter::Parenthesis => {
                args = g.stream();
            }
            _ => return None,
        }
    }
    (chain.last().is_some_and(|l| l == "instrument")).then_some(args)
}

impl Finder<'_> {
    fn instrument(&mut self, tokens: TokenStream) {
        let mut items = Vec::new();
        let parts = segments(tokens);
        let skip_all = parts
            .iter()
            .any(|p| matches!(p.as_slice(), [TokenTree::Ident(i)] if i == "skip_all"));
        let ret = parts
            .iter()
            .any(|p| matches!(p.first(), Some(TokenTree::Ident(i)) if i == "ret"));
        if !skip_all {
            self.problems.push(format!(
                "{} :: {}: #[instrument] without skip_all records every argument with Debug",
                self.file,
                self.scope.path()
            ));
        }
        if ret {
            self.problems.push(format!(
                "{} :: {}: #[instrument(ret)] records the return value",
                self.file,
                self.scope.path()
            ));
        }
        for part in parts {
            match part.first() {
                Some(TokenTree::Ident(i)) if i == "fields" => {
                    if let Some(TokenTree::Group(g)) = part.get(1)
                        && g.delimiter() == Delimiter::Parenthesis
                    {
                        items.extend(read_parts(true, segments(g.stream())));
                    }
                }
                Some(TokenTree::Ident(i)) if i == "skip_all" => {}
                _ => items.push(format!("option {}", compact(part.iter().cloned()))),
            }
        }
        items.sort();
        self.push("instrument".to_owned(), items);
    }
}

fn in_scope(path: &str) -> bool {
    SCOPE
        .iter()
        .any(|s| path == *s || (s.ends_with('/') && path.starts_with(s)))
}

/// Names of the `macro_rules!` anywhere in the tree whose body forwards to a
/// logging macro, directly or through another such macro.
fn wrapper_macros(sources: &[Source]) -> BTreeSet<String> {
    fn bodies(items: &[syn::Item], out: &mut Vec<(String, Vec<String>)>) {
        for item in items {
            match item {
                syn::Item::Macro(m) => {
                    if let Some(ident) = &m.ident {
                        let mut called = Vec::new();
                        macro_calls(m.mac.tokens.clone(), &mut called);
                        out.push((ident.unraw().to_string(), called));
                    }
                }
                syn::Item::Mod(m) => {
                    if let Some((_, inner)) = &m.content {
                        bodies(inner, out);
                    }
                }
                _ => {}
            }
        }
    }
    fn macro_calls(tokens: TokenStream, out: &mut Vec<String>) {
        let all: Vec<TokenTree> = tokens.into_iter().collect();
        for (at, token) in all.iter().enumerate() {
            match token {
                TokenTree::Ident(i) if is_punct(all.get(at + 1), '!') => {
                    out.push(i.unraw().to_string());
                }
                TokenTree::Group(g) => macro_calls(g.stream(), out),
                _ => {}
            }
        }
    }
    let mut all = Vec::new();
    for source in sources {
        bodies(&source.file.items, &mut all);
    }
    let logging =
        |name: &str| EVENTS.contains(&name) || SPANS.contains(&name) || PRINTS.contains(&name);
    let mut wrappers = BTreeSet::new();
    loop {
        let before = wrappers.len();
        for (name, called) in &all {
            if called.iter().any(|c| logging(c) || wrappers.contains(c)) {
                wrappers.insert(name.clone());
            }
        }
        if wrappers.len() == before {
            return wrappers;
        }
    }
}

/// Every logging call in every production file, and the instrument problems.
pub(super) fn scan(sources: &[Source]) -> (Vec<Entry>, Vec<String>) {
    let wrappers = wrapper_macros(sources);
    let mut entries = Vec::new();
    let mut problems = Vec::new();
    for source in sources {
        let mut finder = Finder {
            file: source.path.clone(),
            scope: Scope::default(),
            wrappers: &wrappers,
            entries: Vec::new(),
            problems: Vec::new(),
        };
        finder.visit_file(&source.file);
        entries.extend(finder.entries);
        problems.extend(finder.problems);
    }
    entries.sort();
    (entries, problems)
}

/// The entries in the request path, which the snapshot pins.
pub(super) fn request_path(entries: &[Entry]) -> Vec<Entry> {
    entries
        .iter()
        .filter(|e| in_scope(&e.file))
        .cloned()
        .collect()
}

/// Denied words in an entry's fields and values, minus [`EXEMPT`], and
/// exemptions that match nothing.
pub(super) fn denied_problems(entries: &[Entry], exempt: &[(&str, &str)]) -> Vec<String> {
    let mut out = Vec::new();
    let mut used = BTreeSet::new();
    for entry in entries {
        let line = entry.line();
        for item in &entry.items {
            for word in denied_item(item) {
                if let Some(at) = exempt.iter().position(|(l, w)| *l == line && *w == word) {
                    used.insert(at);
                } else {
                    out.push(format!("{line}: `{item}` names `{word}`"));
                }
            }
        }
    }
    for (at, (line, word)) in exempt.iter().enumerate() {
        if !used.contains(&at) {
            out.push(format!("stale exemption: {line} / {word}"));
        }
    }
    out
}

/// Differences between the scan and a snapshot, as `+`/`-` lines.
pub(super) fn diff(entries: &[Entry], snapshot: &str) -> Vec<String> {
    super::snapshot_diff(entries.iter().map(Entry::line), snapshot)
}

#[cfg(test)]
mod tests {
    use super::super::tree::{parse_fixture, production};
    use super::*;

    fn fixture(path: &str, code: &str) -> (Vec<Entry>, Vec<String>) {
        scan(&[parse_fixture(path, code)])
    }

    fn lines(path: &str, code: &str) -> Vec<String> {
        fixture(path, code).0.iter().map(Entry::line).collect()
    }

    #[test]
    fn logged_values_match_the_reviewed_snapshot() {
        let mut sources = production("lib.rs");
        sources.extend(production("main.rs"));
        let (all, problems) = scan(&sources);
        assert!(problems.is_empty(), "{}", problems.join("\n"));
        let entries = request_path(&all);
        let changes = diff(&entries, SNAPSHOT);
        assert!(
            changes.is_empty(),
            "A logged value changed in the request path (AGENTS.md: never log credentials, \
             inference tokens or prompt/response bodies, #591). Review each line: it must carry \
             an id, a count, a status or a fixed label, never a body, key or token. Then update \
             source_rules/log_fields.snapshot by hand (`+` lines are new, `-` lines are gone):\n{}",
            changes.join("\n")
        );
        assert!(!entries.is_empty(), "the scan found no logging call at all");
    }

    #[test]
    fn no_logged_value_in_any_file_names_a_credential_or_a_body() {
        let mut sources = production("lib.rs");
        sources.extend(production("main.rs"));
        let (entries, _) = scan(&sources);
        assert!(
            entries.len() > request_path(&entries).len(),
            "only the request path was read"
        );
        let found = denied_problems(&entries, EXEMPT);
        assert!(found.is_empty(), "{}", found.join("\n"));
    }

    #[test]
    fn fields_sigils_shorthand_and_format_arguments_are_read() {
        let found = lines(
            "inference/mod.rs",
            r#"impl Engine { fn f(&self, id: u8, user: &str) {
                tracing::warn!(execution_id = %self.id, trace_id = self.t.as_ref().map(T::as_str), ?error, count, "failed {user} {}", id + 1);
            } }"#,
        );
        assert_eq!(
            found,
            vec![
                "inference/mod.rs :: impl Engine::f :: warn :: arg id+1 ; count=count ; \
                 error=?error ; execution_id=%self.id ; fmt user ; \
                 trace_id=self.t.as_ref().map(T::as_str)"
                    .to_owned()
            ]
        );
    }

    #[test]
    fn spans_events_prints_and_nested_macros_are_read() {
        let found = lines(
            "http.rs",
            r#"fn f() {
                let _ = tracing::info_span!("http.request", request_id, method = %m);
                tracing::event!(tracing::Level::INFO, target: "x", a = 1, "msg");
                eprintln!("{x}");
                tokio::select! { _ = fut => { warn!(secret = 1, "x"); } }
            }"#,
        );
        assert_eq!(found.len(), 4, "{found:?}");
        assert!(
            found
                .iter()
                .any(|l| l.contains("info_span :: method=%m ; request_id=request_id"))
        );
        assert!(found.iter().any(|l| l.contains("event :: a=1")));
        assert!(found.iter().any(|l| l.contains("eprintln :: fmt x")));
        assert!(found.iter().any(|l| l.contains("warn :: secret=1")));
    }

    #[test]
    fn a_new_logged_field_shows_in_the_diff() {
        let (entries, _) = fixture(
            "inference/mod.rs",
            r#"fn f() { tracing::info!(request_id = %id, "ok"); }"#,
        );
        let snapshot = "inference/mod.rs :: f :: info :: request_id=%id\n";
        assert!(diff(&entries, snapshot).is_empty());
        let (entries, _) = fixture(
            "inference/mod.rs",
            r#"fn f() { tracing::info!(request_id = %id, prompt = %p, "ok"); }"#,
        );
        let changes = diff(&entries, snapshot);
        assert_eq!(
            changes,
            vec![
                "+ inference/mod.rs :: f :: info :: prompt=%p ; request_id=%id".to_owned(),
                "- inference/mod.rs :: f :: info :: request_id=%id".to_owned(),
            ]
        );
    }

    #[test]
    fn credential_and_body_words_are_refused_in_names_values_and_captures() {
        let (entries, _) = fixture(
            "providers/openai.rs",
            r#"fn f() {
                warn!(body = %b, "x");
                warn!(detail = ?req.messages, "x");
                warn!(a = %api_key, "x");
                warn!(a = %self.apiKey, "x");
                warn!("sent {authorization} {bearerToken}");
                warn!(ok = %execution_id, "x");
                warn!(ok = 1, "the token is fine in fixed text {}", "token");
            }"#,
        );
        let found = denied_problems(&entries, &[]);

        for word in [
            "body",
            "messages",
            "api_key",
            "authorization",
            "bearer",
            "token",
        ] {
            assert!(
                found.iter().any(|p| p.contains(&format!("names `{word}`"))),
                "{word} in {found:?}"
            );
        }
        assert!(
            !found
                .iter()
                .any(|p| p.contains("execution_id") || p.contains("fixed text")),
            "{found:?}"
        );
    }

    #[test]
    fn acronym_spellings_preserve_log_value_refusals() {
        for (field, word) in [("APIkey", "apikey"), ("clientAPIKey", "api_key")] {
            let (entries, _) = fixture(
                "background.rs",
                &format!(r#"fn f() {{ warn!(detail = %self.{field}, "ok"); }}"#),
            );
            let found = denied_problems(&entries, &[]);
            assert!(
                found
                    .iter()
                    .any(|p| p.ends_with(&format!("names `{word}`"))),
                "{found:?}"
            );
        }
    }

    #[test]
    fn every_denied_word_is_refused() {
        for word in DENIED_WORDS {
            let (entries, _) = fixture(
                "inference/x.rs",
                &format!("fn f() {{ warn!(field = %self.{word}, \"x\"); }}"),
            );
            let found = denied_problems(&entries, &[]);
            assert!(
                found.iter().any(|p| p.contains(&format!("names `{word}`"))),
                "{word}: {found:?}"
            );
            let (entries, _) = fixture(
                "inference/x.rs",
                &format!("fn f() {{ warn!({word} = 1, \"x\"); }}"),
            );
            assert!(!denied_problems(&entries, &[]).is_empty(), "name {word}");
        }
        for word in [
            "payload", "response", "request", "headers", "jwt", "creds", "session",
        ] {
            assert!(DENIED_WORDS.contains(&word), "{word}");
        }
        let (entries, _) = fixture(
            "x.rs",
            "fn f() { warn!(a = %self.camelPayloadValue, \"x\"); }",
        );
        assert!(!denied_problems(&entries, &[]).is_empty());
    }

    #[test]
    fn an_exemption_is_exact_and_a_stale_one_fails() {
        let (entries, _) = fixture(
            "http.rs",
            r#"fn f() { info!(status = response.status(), "x"); }"#,
        );
        let line = "http.rs :: f :: info :: status=response.status()";
        assert_eq!(
            denied_problems(&entries, &[]),
            [format!(
                "{line}: `status=response.status()` names `response`"
            )]
        );
        assert!(denied_problems(&entries, &[(line, "response")]).is_empty());
        // A prefix, another word, or another line does not exempt.
        assert_eq!(
            denied_problems(&entries, &[("http.rs :: f", "response")]).len(),
            2
        );
        assert_eq!(denied_problems(&entries, &[(line, "request")]).len(), 2);
        let none = fixture("http.rs", "fn f() { info!(status = 1, \"x\"); }").0;
        assert_eq!(
            denied_problems(&none, &[(line, "response")]),
            [format!("stale exemption: {line} / response")]
        );
    }

    #[test]
    fn wrapper_macros_make_their_call_sites_log_calls() {
        let (entries, _) = fixture(
            "inference/mod.rs",
            r#"macro_rules! note { ($v:expr) => { tracing::info!(value = %$v, "x") }; }
               macro_rules! twice { ($v:expr) => { note!($v) }; }
               fn f(secret: &str) { note!(secret); twice!(self.id); }"#,
        );
        let lines: Vec<String> = entries.iter().map(Entry::line).collect();
        assert!(
            lines.contains(&"inference/mod.rs :: f :: wrapper note :: arg secret".to_owned()),
            "{lines:?}"
        );
        assert!(
            lines.contains(&"inference/mod.rs :: f :: wrapper twice :: arg self.id".to_owned()),
            "{lines:?}"
        );
        let found = denied_problems(&entries, &[]);
        assert!(
            found
                .iter()
                .any(|p| p.contains("wrapper note") && p.contains("names `secret`")),
            "{found:?}"
        );
        // A macro that does not log is not a wrapper.
        let (entries, _) = fixture(
            "inference/mod.rs",
            "macro_rules! plain { ($v:expr) => { $v + 1 }; } fn f() { plain!(1); }",
        );
        assert!(entries.is_empty());
    }

    #[test]
    fn instrument_inside_cfg_attr_is_read_like_instrument() {
        let (entries, problems) = fixture(
            "protocols/chat.rs",
            "#[cfg_attr(feature = \"x\", tracing::instrument)] async fn a(body: Body) {}\n\
             #[cfg_attr(feature = \"x\", instrument(skip(self)))] async fn b(&self) {}\n\
             #[cfg_attr(feature = \"x\", derive(Debug), instrument(skip_all, ret))] async fn c() {}\n\
             #[cfg_attr(feature = \"x\", instrument(skip_all, fields(request_id = %id)))] async fn d() {}\n\
             #[cfg_attr(feature = \"x\", derive(Debug))] struct E;",
        );
        assert_eq!(problems.len(), 3, "{problems:?}");
        assert!(
            problems
                .iter()
                .any(|p| p.contains(":: a:") && p.contains("skip_all"))
        );
        assert!(
            problems
                .iter()
                .any(|p| p.contains(":: b:") && p.contains("skip_all"))
        );
        assert!(
            problems
                .iter()
                .any(|p| p.contains(":: c:") && p.contains("ret"))
        );
        assert_eq!(
            entries.len(),
            4,
            "a, b, c and d are recorded, the derive is not"
        );
        assert!(
            entries
                .iter()
                .any(|e| e.line().contains("instrument :: request_id=%id"))
        );
    }

    #[test]
    fn the_request_path_includes_http_and_every_file_is_scanned_for_denied_words() {
        for path in [
            "http.rs",
            "http/auth.rs",
            "auth.rs",
            "auth/x.rs",
            "jobs/a.rs",
        ] {
            assert!(in_scope(path), "{path}");
        }
        assert!(!in_scope("httpx.rs") && !in_scope("alerts/webhooks.rs"));
        let (entries, _) = fixture("alerts/webhooks.rs", r#"fn f() { warn!(body = %b, "x"); }"#);
        assert!(request_path(&entries).is_empty());
        assert_eq!(denied_problems(&entries, &[]).len(), 1);
        let (entries, _) = fixture("http/auth.rs", r#"fn f() { warn!(job = %b, "x"); }"#);
        assert_eq!(request_path(&entries).len(), 1);
    }

    #[test]
    fn instrument_without_skip_all_or_with_ret_is_refused() {
        let (_, found) = fixture(
            "protocols/chat_completions.rs",
            "#[tracing::instrument] async fn a(body: Body) {}\n\
             #[instrument(skip(self))] async fn b(&self) {}\n\
             #[tracing::instrument(skip_all, ret)] async fn c() {}\n\
             #[instrument(skip_all, fields(request_id = %id))] async fn d() {}",
        );
        assert_eq!(found.len(), 3, "{found:?}");
        assert!(
            found
                .iter()
                .any(|p| p.contains(":: a:") && p.contains("skip_all"))
        );
        assert!(
            found
                .iter()
                .any(|p| p.contains(":: b:") && p.contains("skip_all"))
        );
        assert!(
            found
                .iter()
                .any(|p| p.contains(":: c:") && p.contains("ret"))
        );
    }

    #[test]
    fn instrument_fields_are_recorded() {
        let found = lines(
            "protocols/chat_completions.rs",
            "#[instrument(skip_all, level = \"debug\", fields(request_id = %id, prompt = %p))] async fn d() {}",
        );
        assert_eq!(found.len(), 1);
        assert!(
            found[0].contains("instrument :: option level=\"debug\" ; prompt=%p ; request_id=%id"),
            "{found:?}"
        );
    }

    #[test]
    fn test_code_is_not_scanned() {
        let (entries, _) = fixture(
            "inference/mod.rs",
            r#"#[cfg(test)] mod tests { fn t() { warn!(body = 1, "x"); } }"#,
        );
        assert!(entries.is_empty());
    }

    #[test]
    fn captures_skip_escapes_and_positionals() {
        assert_eq!(captures("a {x} {{y}} {} {0} {z:?} {w:>5}"), ["x", "z", "w"]);
    }
}
