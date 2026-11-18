//! Recognise explicit SQL target names and positional INSERT lists in literal SQL.
use regex::Regex;
use std::sync::LazyLock;

static TOKEN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?s)--[^\n]*|/\*.*?\*/|'(?:''|[^'])*'|"(?:""|[^"])*"|[\w$]+|[^\s]"#).unwrap()
});

fn generated(expr: &[&str]) -> bool {
    expr.windows(3).any(|w| w == ["gen_random_uuid", "(", ")"])
}

// Split an expression list only at its own depth. Nested calls may have commas.
fn list<'a>(tokens: &'a [&'a str]) -> (Vec<&'a [&'a str]>, usize) {
    let mut items = Vec::new();
    let mut start = 0;
    let mut depth = 0;
    for (i, token) in tokens.iter().enumerate() {
        match *token {
            ")" | ";" | "from" | "where" | "returning" | "on" if depth == 0 => {
                items.push(&tokens[start..i]);
                return (items, i);
            }
            "," if depth == 0 => {
                items.push(&tokens[start..i]);
                start = i + 1;
            }
            "(" => depth += 1,
            ")" => depth -= 1,
            _ => {}
        }
    }
    items.push(&tokens[start..]);
    (items, tokens.len())
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
                && generated(value)
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

pub(super) fn scan(path: &str, text: &str, problems: &mut Vec<String>) {
    let lower = text.to_ascii_lowercase();
    if !lower.contains("gen_random_uuid") {
        return;
    }
    let tokens: Vec<_> = TOKEN
        .find_iter(&lower)
        .map(|m| m.as_str())
        .filter(|t| !t.starts_with("--") && !t.starts_with("/*"))
        .collect();
    for stmt in tokens.split(|t| *t == ";") {
        if !generated(stmt) {
            continue;
        }
        let probe = path == "deploy/staging/verify-privileges.sql";
        let mut set = false;
        let mut outbox_update = false;
        let mut report = |name: &str| {
            let name = name.trim_matches('"');
            if super::sensitive(name) {
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
                    set = true;
                    // These staging probes exercise the fence compared alongside lease_owner.
                    outbox_update = probe
                        && (stmt[..i].ends_with(&["update", "alert_webhook_outbox"])
                            || stmt[..i].ends_with(&["update", "alert_recipient_outbox"]));
                }
                "where" | "returning" => set = false,
                "as" if i > 0 && tail.first().is_some_and(|t| identifier(t)) => {
                    // Restrict the alias to its own projection.
                    let start = stmt[..i]
                        .iter()
                        .rposition(|t| matches!(*t, "," | "select"))
                        .map_or(0, |n| n + 1);
                    if generated(&stmt[start..i]) {
                        report(tail[0]);
                    }
                }
                _ if identifier(token) && super::sensitive(token.trim_matches('"')) => {
                    let field = list(tail).0[0];
                    let expression = if set && field.starts_with(&["="]) {
                        Some(&field[1..])
                    } else if i > 0 && matches!(stmt[i - 1], "(" | "," | "column" | "add") {
                        // CREATE/ADD column ... DEFAULT, or ALTER COLUMN name SET DEFAULT.
                        field
                            .iter()
                            .position(|t| *t == "default")
                            .map(|at| &field[at + 1..])
                    } else {
                        None
                    };
                    if expression.is_some_and(generated)
                        && !(set && outbox_update && token == "lease_token")
                    {
                        report(token);
                    }
                }
                _ => {}
            }
        }
    }
}
