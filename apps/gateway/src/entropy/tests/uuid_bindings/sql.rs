//! Recognise explicit SQL target names and positional INSERT lists in literal SQL.
use regex::Regex;
use std::borrow::Cow;
use std::sync::LazyLock;

static TOKEN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?s)--[^\n]*|/\*.*?\*/|'(?:''|[^'])*'|"(?:""|[^"])*"|[\w$]+|[^\s]"#).unwrap()
});

fn generated(expr: &[&str]) -> bool {
    expr.windows(3).any(|w| w == ["gen_random_uuid", "(", ")"])
}

fn ends_clause(tokens: &[&str], at: usize) -> bool {
    match tokens[at] {
        "from" => !tokens[..at].ends_with(&["distinct"]),
        "where" | "returning" | "on" | "into" | "group" | "order" | "having" | "limit"
        | "offset" | "fetch" | "for" | "union" | "intersect" | "except" | "window" => true,
        _ => false,
    }
}

// A scalar SELECT contributes its projection. Source rows, predicates and
// ordering may mention UUIDs without supplying the bound value.
fn generated_value(tokens: &[&str]) -> bool {
    let mut parents = Vec::new();
    let (mut select, mut visible) = (false, true);
    for (at, &token) in tokens.iter().enumerate() {
        match token {
            "(" | "[" => {
                parents.push((select, visible));
                select = false;
            }
            ")" | "]" => (select, visible) = parents.pop().unwrap_or((false, true)),
            "select" => select = true,
            _ if select && ends_clause(tokens, at) => visible = false,
            "gen_random_uuid" if visible && tokens[at + 1..].starts_with(&["(", ")"]) => {
                return true;
            }
            _ => {}
        }
    }
    false
}

fn expression_len(tokens: &[&str], column_default: bool) -> usize {
    let mut depth = 0;
    for (at, &token) in tokens.iter().enumerate() {
        if depth == 0
            && (matches!(token, "," | ")" | "]" | ";")
                || ends_clause(tokens, at)
                || column_default
                    && matches!(
                        token,
                        "constraint"
                            | "check"
                            | "primary"
                            | "unique"
                            | "references"
                            | "generated"
                            | "collate"
                    ))
        {
            return at;
        }
        match token {
            "(" | "[" => depth += 1,
            ")" | "]" => depth -= 1,
            _ => {}
        }
    }
    tokens.len()
}

fn list<'a>(tokens: &'a [&'a str]) -> (Vec<&'a [&'a str]>, usize) {
    let mut items = Vec::new();
    let mut start = 0;
    loop {
        let end = start + expression_len(&tokens[start..], false);
        items.push(&tokens[start..end]);
        if tokens.get(end) != Some(&",") {
            return (items, end);
        }
        start = end + 1;
    }
}

fn insert(tokens: &[&str], mut report: impl FnMut(&str)) {
    // INSERT INTO [schema.]table [(columns)] VALUES (...), (...) or SELECT ...
    let Some(open) = tokens
        .iter()
        .position(|t| !matches!(*t, ".") && !identifier(t))
    else {
        return;
    };
    if tokens[open] != "(" {
        return;
    }
    let (columns, end) = list(&tokens[open + 1..]);
    let Some(mut values) = tokens.get(open + end + 2..) else {
        return;
    };
    let select = values.first() == Some(&"select");
    if !select && values.first() != Some(&"values") {
        return;
    }
    values = &values[1..];
    loop {
        if !select {
            let Some(rest) = values.strip_prefix(&["("]) else {
                break;
            };
            values = rest;
        }
        let (items, end) = list(values);
        for (column, value) in columns.iter().zip(items) {
            if let [name] = *column
                && generated_value(value)
            {
                report(name);
            }
        }
        if select {
            break;
        }
        let Some(rest) = values[end..].strip_prefix(&[")", ","]) else {
            break;
        };
        values = rest;
    }
}

fn identifier(token: &str) -> bool {
    token.starts_with('"') || token.starts_with(|c: char| c == '_' || c.is_alphabetic())
}

fn identifier_name(token: &str) -> Cow<'_, str> {
    if let Some(inner) = token.strip_prefix('"').and_then(|s| s.strip_suffix('"')) {
        return inner.replace("\"\"", "\"").into();
    }
    token.into()
}

pub(super) fn scan(path: &str, text: &str, problems: &mut Vec<String>) {
    let lower = text.to_ascii_lowercase();
    if !lower.contains("gen_random_uuid") {
        return;
    }
    let tokens: Vec<_> = TOKEN
        .find_iter(&lower)
        .map(|m| {
            // Offsets agree because ASCII case folding preserves byte length. Quoted
            // PostgreSQL identifiers keep their case, including for exact exceptions.
            if m.as_str().starts_with('"') {
                &text[m.range()]
            } else {
                m.as_str()
            }
        })
        .filter(|t| !t.starts_with("--") && !t.starts_with("/*"))
        .collect();
    for stmt in tokens.split(|t| *t == ";") {
        if !generated(stmt) {
            continue;
        }
        let probe = path == "deploy/staging/verify-privileges.sql";
        let mut report = |name: &str| {
            let name = identifier_name(name);
            if super::sensitive(&name) {
                problems.push(format!("{path}: gen_random_uuid bound to {name}"));
            }
        };
        for (i, &token) in stmt.iter().enumerate() {
            let tail = &stmt[i + 1..];
            match token {
                "into" if i > 0 && stmt[i - 1] == "insert" => insert(tail, |name| {
                    // The report-run privilege probe supplies the generation claim fence.
                    if !(probe && tail.starts_with(&["report_runs", "("]) && name == "claim_token")
                    {
                        report(name);
                    }
                }),
                "set" => {
                    // An inner WHERE belongs to its subquery. Each outer assignment
                    // ends at the next comma or clause at the SET list's own depth.
                    let fence_probe = probe
                        && (stmt[..i].ends_with(&["update", "alert_webhook_outbox"])
                            || stmt[..i].ends_with(&["update", "alert_recipient_outbox"]));
                    for assignment in list(tail).0 {
                        if let [name, "=", value @ ..] = assignment
                            && identifier(name)
                            && generated_value(value)
                            && !(fence_probe && *name == "lease_token")
                        {
                            report(name);
                        }
                    }
                }
                "select" | "returning" => {
                    for projection in list(tail).0 {
                        if let [value @ .., "as", name] = projection
                            && identifier(name)
                            && generated_value(value)
                        {
                            report(name);
                        }
                    }
                }
                // A column name follows `(`, `,`, ADD [COLUMN] [IF NOT EXISTS] or
                // ALTER [COLUMN]. `CREATE TABLE IF NOT EXISTS name` and
                // `ALTER TABLE name` name a table: neither is preceded by ADD or
                // COLUMN, nor followed by SET DEFAULT.
                _ if i > 0
                    && (matches!(stmt[i - 1], "(" | "," | "column" | "add")
                        || (stmt[..i].ends_with(&["if", "not", "exists"])
                            && i > 3
                            && matches!(stmt[i - 4], "column" | "add"))
                        || (stmt[i - 1] == "alter" && tail.starts_with(&["set", "default"])))
                    && identifier(token)
                    && super::sensitive(&identifier_name(token)) =>
                {
                    // CREATE/ADD column ... DEFAULT, or ALTER COLUMN name SET DEFAULT.
                    let field = &tail[..expression_len(tail, false)];
                    let expression = field
                        .iter()
                        .position(|t| *t == "default")
                        .map(|at| &field[at + 1..]);
                    if expression
                        .is_some_and(|value| generated_value(&value[..expression_len(value, true)]))
                    {
                        report(token);
                    }
                }
                _ => {}
            }
        }
    }
}
