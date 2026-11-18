//! Recognise explicit SQL target names and positional INSERT lists in literal SQL.
use regex::Regex;
use std::sync::LazyLock;

static TOKEN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?s)--[^\n]*|/\*.*?\*/|'(?:''|[^'])*'|"(?:""|[^"])*"|[\w$]+|[^\s]"#).unwrap()
});

fn generated(expr: &[&str]) -> bool {
    expr.windows(3).any(|w| w == ["gen_random_uuid", "(", ")"])
}

// Stop at the next delimiter at this expression's own depth.
fn expression_len(tokens: &[&str]) -> usize {
    let mut depth = 0;
    for (i, token) in tokens.iter().enumerate() {
        match *token {
            "," | ")" | ";" | "from" | "where" | "returning" | "on" | "into" | "group"
            | "order" | "having" | "limit" | "union" | "intersect" | "except"
                if depth == 0 =>
            {
                return i;
            }
            "(" => depth += 1,
            ")" => depth -= 1,
            _ => {}
        }
    }
    tokens.len()
}

fn list<'a>(tokens: &'a [&'a str]) -> (Vec<&'a [&'a str]>, usize) {
    let mut items = Vec::new();
    let mut start = 0;
    loop {
        let end = start + expression_len(&tokens[start..]);
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
                    // An inner WHERE belongs to its subquery. Each outer assignment
                    // ends at the next comma or clause at the SET list's own depth.
                    let fence_probe = probe
                        && (stmt[..i].ends_with(&["update", "alert_webhook_outbox"])
                            || stmt[..i].ends_with(&["update", "alert_recipient_outbox"]));
                    for assignment in list(tail).0 {
                        if let [name, "=", value @ ..] = assignment
                            && identifier(name)
                            && generated(value)
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
                            && generated(value)
                        {
                            report(name);
                        }
                    }
                }
                _ if i > 0
                    && matches!(stmt[i - 1], "(" | "," | "column" | "add")
                    && identifier(token)
                    && super::sensitive(token.trim_matches('"')) =>
                {
                    // CREATE/ADD column ... DEFAULT, or ALTER COLUMN name SET DEFAULT.
                    let field = &tail[..expression_len(tail)];
                    let expression = field
                        .iter()
                        .position(|t| *t == "default")
                        .map(|at| &field[at + 1..]);
                    if expression.is_some_and(generated) {
                        report(token);
                    }
                }
                _ => {}
            }
        }
    }
}
