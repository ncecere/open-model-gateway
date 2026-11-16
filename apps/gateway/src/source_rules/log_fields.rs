//! Never log plaintext credentials, inference tokens or prompt/response bodies
//! (AGENTS.md invariant, #591). In the request-handling code, every value a
//! logging call can carry is pinned to a reviewed snapshot, so a new logged
//! field shows up in review and cannot arrive by accident.
//!
//! Scanned (production build only): `inference/`, `providers/`, `protocols/`,
//! `jobs/`, `http.rs`, `auth.rs` and `auth/` ([`SCOPE`]). For every
//! `tracing` event or span macro (`trace!`..`error!`, `event!`, `span!`,
//! `*_span!`, as `tracing::x!` or bare, also inside the body of any other
//! macro), `#[instrument]` attribute and `print!`-family macro the test
//! records the macro, its function, each *field* (`name = value` with its
//! `%`/`?` sigil, or a shorthand `name`), and each *format argument*
//! (inline `{name}` captures and positional or named arguments), as
//! whitespace-free token text. The sorted list must equal
//! `log_fields.snapshot` line for line. Adding, removing or changing a logged
//! value is a diff of that file.
//!
//! Two rules need no snapshot entry to fail:
//!
//! * an identifier or field name made of a word from [`DENIED_WORDS`]
//!   (`body`, `prompt`, `content`, `messages`, `token`, `secret`,
//!   `credential`, `password`, `authorization`, `bearer`, `cookie`, ...) or
//!   `api` + `key` anywhere in a logged value is refused, unless the exact
//!   (entry, word) is in [`EXEMPT`];
//! * `#[instrument]` without `skip_all` (it records every argument with
//!   `Debug`) and `ret` (it records the return value) are refused.
//!
//! Cannot catch: a value formatted into an allowed field (`error = ?e` is a
//! `Debug` of whatever `e` holds, and `%path` of a path that embeds a secret);
//! a `Debug`/`Display` impl that prints a secret field (see the redaction
//! tests of the types themselves); a logging macro imported under another
//! name (`use tracing::warn as w;`); logging through `ops_log` events, the
//! `log` crate, writers or the metrics labels (each has its own tests);
//! files outside [`SCOPE`]; `Span::record` calls.
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
];

/// (snapshot entry prefix, word): reviewed uses of a denied word. Empty: no
/// logged value names one today.
const EXEMPT: &[(&str, &str)] = &[];

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

fn words(ident: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut word = String::new();
    let mut previous_lower = false;
    for c in ident.chars() {
        if !c.is_alphanumeric() {
            if !word.is_empty() {
                out.push(std::mem::take(&mut word));
            }
            previous_lower = false;
            continue;
        }
        if c.is_uppercase() && previous_lower && !word.is_empty() {
            out.push(std::mem::take(&mut word));
        }
        previous_lower = c.is_lowercase() || c.is_ascii_digit();
        word.extend(c.to_lowercase());
    }
    if !word.is_empty() {
        out.push(word);
    }
    out
}

/// Denied words in a stretch of token text (a name or an expression).
fn denied(text: &str) -> BTreeSet<String> {
    let all = words(text);
    let mut out: BTreeSet<String> = all
        .iter()
        .filter(|w| DENIED_WORDS.contains(&w.as_str()))
        .cloned()
        .collect();
    if all.windows(2).any(|p| p[0] == "api" && p[1] == "key") {
        out.insert("api_key".to_owned());
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

struct Finder {
    file: String,
    scope: Scope,
    entries: Vec<Entry>,
    problems: Vec<String>,
}

impl Finder {
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
        }
        // A logging call can sit inside the arguments of another macro.
        self.tokens(tokens);
    }
}

impl<'ast> Visit<'ast> for Finder {
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
        if last.ident != "instrument" {
            return;
        }
        let mut items = Vec::new();
        let tokens = match &attr.meta {
            syn::Meta::List(list) => list.tokens.clone(),
            _ => TokenStream::new(),
        };
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

/// Every logging call in the scanned files, and the instrument problems.
pub(super) fn scan(sources: &[Source]) -> (Vec<Entry>, Vec<String>) {
    let mut entries = Vec::new();
    let mut problems = Vec::new();
    for source in sources.iter().filter(|s| in_scope(&s.path)) {
        let mut finder = Finder {
            file: source.path.clone(),
            scope: Scope::default(),
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

/// Denied words in an entry's fields and values, minus [`EXEMPT`].
pub(super) fn denied_problems(entries: &[Entry], exempt: &[(&str, &str)]) -> Vec<String> {
    let mut out = Vec::new();
    for entry in entries {
        let line = entry.line();
        for item in &entry.items {
            for word in denied(item) {
                if !exempt
                    .iter()
                    .any(|(prefix, w)| line.starts_with(prefix) && *w == word)
                {
                    out.push(format!("{line}: `{item}` names `{word}`"));
                }
            }
        }
    }
    out
}

/// Differences between the scan and a snapshot, as `+`/`-` lines.
pub(super) fn diff(entries: &[Entry], snapshot: &str) -> Vec<String> {
    let have: Vec<String> = entries.iter().map(Entry::line).collect();
    let want: Vec<String> = snapshot
        .lines()
        .filter(|l| !l.trim().is_empty() && !l.starts_with('#'))
        .map(str::to_owned)
        .collect();
    let mut remaining = want.clone();
    let mut out = Vec::new();
    for line in &have {
        if let Some(at) = remaining.iter().position(|w| w == line) {
            remaining.remove(at);
        } else {
            out.push(format!("+ {line}"));
        }
    }
    out.extend(remaining.into_iter().map(|l| format!("- {l}")));
    out
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
        let (entries, problems) = scan(&sources);
        assert!(problems.is_empty(), "{}", problems.join("\n"));
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
    fn no_logged_value_names_a_credential_or_a_body() {
        let sources = production("lib.rs");
        let (entries, _) = scan(&sources);
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
            !found.iter().any(|p| p.contains("execution_id")),
            "{found:?}"
        );
        let exempt = [("providers/openai.rs :: f :: warn :: body=%b", "body")];
        let after = denied_problems(&entries, &exempt);
        assert_eq!(after.len(), found.len() - 1);
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
    fn files_outside_the_request_path_and_test_code_are_not_scanned() {
        assert!(!in_scope("alerts/webhooks.rs"));
        assert!(in_scope("inference/attempt.rs") && in_scope("http.rs") && in_scope("auth/x.rs"));
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
