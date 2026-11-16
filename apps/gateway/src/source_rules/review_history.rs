//! Comments state invariants, not review history (AGENTS.md, #595). A comment
//! that records who asked for a change and when ("#98 PR 2 review item 5:",
//! "found in the review of #37") goes stale and hands the next agent a
//! justification to copy instead of a rule to follow; git history and the PR
//! record why. This scan refuses such comments in the production sources of
//! the crate (`lib.rs` and `main.rs` and every module below them, as
//! [`super::tree`] reads them; `cfg(test)` code is out of scope, so are
//! `tests/`, `benches/` and files only a test build reaches).
//!
//! Comments are read from the source text with a small Rust lexer, not by
//! line patterns: a string, raw string, byte string or character literal that
//! holds `//` or the words is not a comment; every `//`, `///`, `//!`, `/* */`,
//! `/** */` and `/*! */` is, doc comments included (`syn` keeps those as
//! attributes and drops the others, so it cannot be the reader). Items `syn`
//! prunes as test-only are located by span and their comments skipped.
//! `#[doc = "..."]` written as an attribute is a string, not a comment, and is
//! not read.
//!
//! The patterns live in `review_history.patterns`, shared with the web check
//! (`apps/web/src/lib/review-history.test.ts`); the format, what is refused and
//! what is deliberately allowed are described there. Each line of the file
//! marked `refuse` or `allow` runs here as a planted comment, in every comment
//! form, before the tree is scanned.
//!
//! A hit that cannot be told from product vocabulary mechanically goes on
//! [`ALLOWED`] with its reason; a stale entry fails. The list is empty.
//!
//! Cannot see: history written without one of the phrasings (a free-form
//! "after the last round of changes"), a comment inside a macro invocation
//! that the macro turns into code, history in a string that becomes a doc
//! (`#[doc = ...]`), and comments in non-Rust files (SQL migrations, YAML,
//! docs).
use std::collections::BTreeSet;

use super::tree::{dropped_spans, production, src_dir};

const PATTERNS: &str = include_str!("review_history.patterns");

/// (file, comment text as scanned, reason): comments that match a pattern and
/// are not history. The text is the whole comment, whitespace collapsed.
const ALLOWED: &[(&str, &str, &str)] = &[];

// ---- patterns ----------------------------------------------------------

#[derive(Debug)]
enum Element {
    Words {
        words: Vec<String>,
        optional: bool,
    },
    Number,
    Skip(usize),
    /// The word before this position is none of these.
    NotBefore(Vec<String>),
}

#[derive(Debug)]
enum Pattern {
    Words(String, Vec<Element>),
    Raw(String),
}

impl Pattern {
    fn source(&self) -> &str {
        match self {
            Pattern::Words(source, _) => source,
            Pattern::Raw(text) => text,
        }
    }
}

struct Rules {
    patterns: Vec<Pattern>,
    refuse: Vec<String>,
    allow: Vec<String>,
}

fn parse_rules(text: &str) -> Rules {
    let mut rules = Rules {
        patterns: Vec::new(),
        refuse: Vec::new(),
        allow: Vec::new(),
    };
    for line in text.lines() {
        let line = line.trim_end();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (kind, rest) = line
            .split_once(' ')
            .unwrap_or_else(|| panic!("review_history.patterns: unreadable line {line:?}"));
        match kind {
            "pattern" => {
                let elements = rest
                    .split_whitespace()
                    .map(|element| {
                        let (body, optional) = match element.strip_suffix('?') {
                            Some(body) => (body, true),
                            None => (element, false),
                        };
                        if let Some(count) = body.strip_prefix("..") {
                            Element::Skip(count.parse().expect("skip count"))
                        } else if body == "<n>" {
                            Element::Number
                        } else if let Some(set) = body.strip_prefix('^') {
                            Element::NotBefore(set.split('|').map(str::to_owned).collect())
                        } else {
                            Element::Words {
                                words: body.split('|').map(str::to_owned).collect(),
                                optional,
                            }
                        }
                    })
                    .collect();
                rules
                    .patterns
                    .push(Pattern::Words(rest.to_owned(), elements));
            }
            "raw" => rules.patterns.push(Pattern::Raw(rest.to_owned())),
            "refuse" => rules.refuse.push(rest.to_owned()),
            "allow" => rules.allow.push(rest.to_owned()),
            other => panic!("review_history.patterns: unknown line kind {other:?}"),
        }
    }
    rules
}

/// Lowercase words: runs of ASCII letters, runs of ASCII digits, `#` as `@`.
fn words(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut current = String::new();
    let flush = |current: &mut String, out: &mut Vec<String>| {
        if !current.is_empty() {
            out.push(std::mem::take(current));
        }
    };
    for c in text.chars() {
        let c = c.to_ascii_lowercase();
        let class = |c: char| {
            if c.is_ascii_lowercase() {
                1
            } else if c.is_ascii_digit() {
                2
            } else {
                0
            }
        };
        if c == '#' {
            flush(&mut current, &mut out);
            out.push("@".to_owned());
        } else if class(c) == 0 {
            flush(&mut current, &mut out);
        } else {
            if let Some(last) = current.chars().last()
                && class(last) != class(c)
            {
                flush(&mut current, &mut out);
            }
            current.push(c);
        }
    }
    flush(&mut current, &mut out);
    out
}

fn matches_from(elements: &[Element], at: usize, words: &[String]) -> bool {
    let Some((first, rest)) = elements.split_first() else {
        return true;
    };
    match first {
        Element::Skip(most) => (0..=*most).any(|n| matches_from(rest, at + n, words)),
        Element::NotBefore(set) => {
            (at == 0 || !set.contains(&words[at - 1])) && matches_from(rest, at, words)
        }
        Element::Number => {
            at < words.len()
                && words[at].bytes().all(|b| b.is_ascii_digit())
                && matches_from(rest, at + 1, words)
        }
        Element::Words {
            words: alternatives,
            optional,
        } => {
            (at < words.len()
                && alternatives.contains(&words[at])
                && matches_from(rest, at + 1, words))
                || (*optional && matches_from(rest, at, words))
        }
    }
}

/// The patterns that refuse `comment` (whitespace collapsed by the caller).
fn refused_by<'a>(rules: &'a Rules, comment: &str) -> Vec<&'a str> {
    let words = words(comment);
    let lower = comment.to_lowercase();
    rules
        .patterns
        .iter()
        .filter(|pattern| match pattern {
            Pattern::Words(_, elements) => {
                (0..words.len()).any(|start| matches_from(elements, start, &words))
            }
            Pattern::Raw(text) => lower.contains(text.as_str()),
        })
        .map(Pattern::source)
        .collect()
}

// ---- comments ----------------------------------------------------------

struct Comment {
    line: usize,
    column: usize,
    end_line: usize,
    /// Starts a `//` comment.
    line_comment: bool,
    /// Nothing but whitespace before it on its line.
    own_line: bool,
    text: String,
}

/// Every comment of a Rust source text, in order, with its position
/// (line from 1, column from 0 in characters, like `proc_macro2`).
fn comments(source: &str) -> Vec<Comment> {
    let chars: Vec<char> = source.chars().collect();
    let mut out = Vec::new();
    let (mut i, mut line, mut column) = (0usize, 1usize, 0usize);
    let mut code_on_line = false;
    // Consume `n` characters, tracking position.
    macro_rules! advance {
        ($n:expr) => {
            for _ in 0..$n {
                if chars[i] == '\n' {
                    line += 1;
                    column = 0;
                    code_on_line = false;
                } else {
                    column += 1;
                }
                i += 1;
            }
        };
    }
    let at = |i: usize| chars.get(i).copied().unwrap_or('\0');
    while i < chars.len() {
        let c = chars[i];
        if c == '/' && at(i + 1) == '/' {
            let start = (line, column, !code_on_line);
            let begin = i + 2;
            while i < chars.len() && chars[i] != '\n' {
                advance!(1);
            }
            out.push(Comment {
                line: start.0,
                column: start.1,
                end_line: start.0,
                line_comment: true,
                own_line: start.2,
                text: chars[begin..i].iter().collect(),
            });
        } else if c == '/' && at(i + 1) == '*' {
            let start = (line, column, !code_on_line);
            let begin = i + 2;
            let mut depth = 0;
            loop {
                if i >= chars.len() {
                    break;
                }
                if chars[i] == '/' && at(i + 1) == '*' {
                    depth += 1;
                    advance!(2);
                } else if chars[i] == '*' && at(i + 1) == '/' {
                    depth -= 1;
                    advance!(2);
                    if depth == 0 {
                        break;
                    }
                } else {
                    advance!(1);
                }
            }
            let end = i.saturating_sub(2).max(begin);
            out.push(Comment {
                line: start.0,
                column: start.1,
                end_line: line,
                line_comment: false,
                own_line: start.2,
                text: chars[begin..end].iter().collect(),
            });
            // The end of a block comment is not code.
        } else if c == '"' {
            code_on_line = true;
            advance!(1);
            skip_string(&chars, &mut i, &mut line, &mut column);
        } else if c == '\'' {
            code_on_line = true;
            if at(i + 1) == '\\' {
                // '\n', '\'', '\u{..}': to the closing quote after the escape.
                advance!(3);
                while i < chars.len() && chars[i] != '\'' {
                    advance!(1);
                }
                advance!(1);
            } else if at(i + 2) == '\'' {
                advance!(3);
            } else {
                // A lifetime or loop label.
                advance!(1);
            }
        } else if c.is_alphabetic() || c == '_' {
            code_on_line = true;
            let begin = i;
            while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                advance!(1);
            }
            let ident: String = chars[begin..i].iter().collect();
            match (ident.as_str(), at(i)) {
                ("r" | "br" | "cr", '"' | '#') => {
                    let mut hashes = 0;
                    while at(i + hashes) == '#' {
                        hashes += 1;
                    }
                    if at(i + hashes) == '"' {
                        advance!(hashes + 1);
                        // To `"` followed by the same number of `#`.
                        while i < chars.len() {
                            if chars[i] == '"' && (1..=hashes).all(|k| at(i + k) == '#') {
                                advance!(hashes + 1);
                                break;
                            }
                            advance!(1);
                        }
                    }
                    // Otherwise a raw identifier (`r#type`): lexed as itself.
                }
                ("b" | "c", '"') => {
                    advance!(1);
                    skip_string(&chars, &mut i, &mut line, &mut column);
                }
                ("b", '\'') => {
                    // b'x', b'\n', b'\'': the closing quote follows the
                    // escape or the one character.
                    advance!(1);
                    if at(i) == '\\' {
                        advance!(2);
                    } else {
                        advance!(1);
                    }
                    while i < chars.len() && chars[i] != '\'' {
                        advance!(1);
                    }
                    advance!(1);
                }
                _ => {}
            }
        } else {
            if !c.is_whitespace() {
                code_on_line = true;
            }
            advance!(1);
        }
    }
    out
}

/// After the opening quote: to the closing one, honouring `\` escapes.
fn skip_string(chars: &[char], i: &mut usize, line: &mut usize, column: &mut usize) {
    while *i < chars.len() {
        let c = chars[*i];
        *i += 1;
        if c == '\n' {
            *line += 1;
            *column = 0;
        } else {
            *column += 1;
        }
        if c == '\\' && *i < chars.len() {
            if chars[*i] == '\n' {
                *line += 1;
                *column = 0;
            } else {
                *column += 1;
            }
            *i += 1;
        } else if c == '"' {
            return;
        }
    }
}

/// A comment as the patterns read it: adjacent own-line `//` comments are one.
struct Block {
    line: usize,
    text: String,
}

fn blocks(comments: Vec<Comment>) -> Vec<Block> {
    let mut out: Vec<Block> = Vec::new();
    let mut last_end = 0;
    let mut last_own_line_comment = false;
    for comment in comments {
        let join = comment.line_comment
            && comment.own_line
            && last_own_line_comment
            && comment.line == last_end + 1;
        last_end = comment.end_line;
        last_own_line_comment = comment.line_comment && comment.own_line;
        let text = comment.text;
        if join {
            let block = out.last_mut().expect("joined to a block");
            block.text.push(' ');
            block.text.push_str(&text);
        } else {
            out.push(Block {
                line: comment.line,
                text,
            });
        }
    }
    for block in &mut out {
        block.text = block.text.split_whitespace().collect::<Vec<_>>().join(" ");
    }
    out
}

/// One refused comment.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Hit {
    file: String,
    line: usize,
    patterns: Vec<String>,
    text: String,
}

/// The refused comments of one production file's text, leaving out comments
/// inside items a production build drops (`cfg(test)`, `#[test]`).
fn scan(rules: &Rules, file: &str, source: &str) -> (usize, Vec<Hit>) {
    let parsed = syn::parse_file(source).unwrap_or_else(|e| panic!("{file}: {e}"));
    let dropped = dropped_spans(&parsed);
    let all = comments(source);
    let kept: Vec<Comment> = all
        .into_iter()
        .filter(|c| {
            let start = (c.line, c.column);
            !dropped.iter().any(|(a, b)| *a <= start && start <= *b)
        })
        .collect();
    let blocks = blocks(kept);
    let count = blocks.len();
    let hits = blocks
        .into_iter()
        .filter_map(|block| {
            let patterns = refused_by(rules, &block.text);
            (!patterns.is_empty()).then(|| Hit {
                file: file.to_owned(),
                line: block.line,
                patterns: patterns.into_iter().map(str::to_owned).collect(),
                text: block.text,
            })
        })
        .collect();
    (count, hits)
}

fn is_allowed(hit: &Hit) -> bool {
    ALLOWED
        .iter()
        .any(|(file, text, _)| *file == hit.file && *text == hit.text)
}

/// (files, comment blocks, hits) of the production tree.
fn scan_tree(rules: &Rules) -> (usize, usize, Vec<Hit>) {
    let src = src_dir();
    let mut files = BTreeSet::new();
    for entry in ["lib.rs", "main.rs"] {
        for source in production(entry) {
            files.insert(source.path);
        }
    }
    let (mut blocks, mut hits) = (0, Vec::new());
    for file in &files {
        let text =
            std::fs::read_to_string(src.join(file)).unwrap_or_else(|e| panic!("{file}: {e}"));
        let (count, found) = scan(rules, file, &text);
        blocks += count;
        hits.extend(found);
    }
    (files.len(), blocks, hits)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules() -> Rules {
        parse_rules(PATTERNS)
    }

    fn refused(rules: &Rules, comment: &str) -> bool {
        !refused_by(
            rules,
            &comment.split_whitespace().collect::<Vec<_>>().join(" "),
        )
        .is_empty()
    }

    /// `comment` written as every form of Rust comment, once as a trailing
    /// comment after code and once wrapped over several lines.
    fn forms(comment: &str) -> Vec<String> {
        let words: Vec<&str> = comment.split(' ').collect();
        let middle = words.len() / 2;
        let wrapped = format!(
            "// {}\n// {}\nfn f() {{}}\n",
            words[..middle].join(" "),
            words[middle..].join(" ")
        );
        let wrapped_doc = format!(
            "/// {}\n/// {}\nfn f() {{}}\n",
            words[..middle].join(" "),
            words[middle..].join(" ")
        );
        let block_wrapped = format!(
            "/*\n * {}\n * {}\n */\nfn f() {{}}\n",
            words[..middle].join(" "),
            words[middle..].join(" ")
        );
        vec![
            format!("// {comment}\nfn f() {{}}\n"),
            format!("/// {comment}\nfn f() {{}}\n"),
            format!("//! {comment}\nfn f() {{}}\n"),
            format!("/* {comment} */\nfn f() {{}}\n"),
            format!("/** {comment} */\nfn f() {{}}\n"),
            format!("/*! {comment} */\nfn f() {{}}\n"),
            format!("fn f() {{}} // {comment}\n"),
            format!("fn f() {{\n    let x = 1; /* {comment} */\n}}\n"),
            format!("mod m {{\n    /// {comment}\n    pub fn f() {{}}\n}}\n"),
            wrapped,
            wrapped_doc,
            block_wrapped,
        ]
    }

    #[test]
    fn the_pattern_file_parses_and_has_samples() {
        let rules = rules();
        assert!(rules.patterns.len() >= 15, "{}", rules.patterns.len());
        assert!(rules.refuse.len() >= 25, "{}", rules.refuse.len());
        assert!(rules.allow.len() >= 20, "{}", rules.allow.len());
    }

    #[test]
    fn every_refused_sample_is_found_in_every_comment_form() {
        let rules = rules();
        for sample in &rules.refuse {
            assert!(refused(&rules, sample), "not refused as text: {sample}");
            for source in forms(sample) {
                let (blocks, hits) = scan(&rules, "planted.rs", &source);
                assert!(blocks >= 1, "no comment read in {source:?}");
                assert_eq!(hits.len(), 1, "sample {sample:?} in {source:?}: {hits:?}");
            }
        }
    }

    #[test]
    fn every_allowed_sample_passes_in_every_comment_form() {
        let rules = rules();
        for sample in &rules.allow {
            for source in forms(sample) {
                let (_, hits) = scan(&rules, "planted.rs", &source);
                assert!(hits.is_empty(), "sample {sample:?} in {source:?}: {hits:?}");
            }
        }
    }

    #[test]
    fn each_pattern_refuses_a_sample_of_its_own() {
        let rules = rules();
        for pattern in &rules.patterns {
            let source = pattern.source();
            assert!(
                rules.refuse.iter().any(|sample| {
                    refused_by(
                        &rules,
                        &sample.split_whitespace().collect::<Vec<_>>().join(" "),
                    )
                    .contains(&source)
                }),
                "no refuse sample exercises: {source}"
            );
        }
    }

    #[test]
    fn a_literal_is_not_a_comment() {
        let rules = rules();
        let source = r##"
fn f() -> usize {
    let a = "see // #98 PR 2 review item 5";
    let b = "a /* review round 2 */ b";
    let c = r#"// review item 3 "# ;
    let d = r"// review item 4";
    let e = b"// review item 5";
    let f = c"/* reviewer asked */";
    let g = '"'; // fine
    let h = '\'';
    let i = b'"';
    let j = "escaped \" // review item 6 \\";
    let k = "line one
        // review item 7 inside a string";
    let l: &'static str = "x";
    'outer: loop { break 'outer; }
    let r#type = 1;
    a.len() + r#type
}
"##;
        let (blocks, hits) = scan(&rules, "literals.rs", source);
        assert!(hits.is_empty(), "{hits:?}");
        assert_eq!(blocks, 1, "only the `// fine` comment");
    }

    #[test]
    fn a_comment_after_a_literal_is_still_read() {
        let rules = rules();
        for source in [
            "fn f() { let a = \"x\"; // #98 PR 2 review item 5\n}\n",
            "fn f() { let a = '\"'; // #98 PR 2 review item 5\n}\n",
            "fn f() { let a = r#\"x\"#; // #98 PR 2 review item 5\n}\n",
            "fn f<'a>(x: &'a str) {} // #98 PR 2 review item 5\n",
            "fn f() { let a = '\\''; /* #98 PR 2 review item 5 */ }\n",
            "/* outer /* nested */ #98 PR 2 review item 5 */\nfn f() {}\n",
        ] {
            let (_, hits) = scan(&rules, "after.rs", source);
            assert_eq!(hits.len(), 1, "{source:?}: {hits:?}");
        }
    }

    #[test]
    fn positions_count_lines_and_characters() {
        let found = comments("fn é() {} // a\n/* b\nc */ fn g() {} // d\n");
        let at: Vec<(usize, usize, usize)> = found
            .iter()
            .map(|c| (c.line, c.column, c.end_line))
            .collect();
        assert_eq!(at, vec![(1, 10, 1), (2, 0, 3), (3, 15, 3)]);
        assert!(!found[0].own_line && found[1].own_line && !found[2].own_line);
    }

    #[test]
    fn only_adjacent_own_line_line_comments_are_one_comment() {
        let rules = rules();
        // Split across two own-line comments: one comment, refused.
        let (_, hits) = scan(&rules, "a.rs", "// review\n// item 5\nfn f() {}\n");
        assert_eq!(hits.len(), 1);
        // A blank line, a trailing comment, or a block comment ends it.
        for source in [
            "// review\n\n// item 5\nfn f() {}\n",
            "fn f() {} // review\n// item 5\nfn g() {}\n",
            "// review\nfn f() {}\n// item 5\n",
            "/* review */\n// item 5\nfn f() {}\n",
            "// review\n/* item 5 */\nfn f() {}\n",
        ] {
            let (_, hits) = scan(&rules, "a.rs", source);
            assert!(hits.is_empty(), "{source:?}: {hits:?}");
        }
    }

    #[test]
    fn test_only_code_is_out_of_scope_and_the_rest_is_not() {
        let rules = rules();
        let planted = "// #98 PR 2 review item 5: planted";
        for (source, expected) in [
            (
                format!("#[cfg(test)]\nmod tests {{\n    {planted}\n    fn t() {{}}\n}}\n"),
                0,
            ),
            (format!("#[cfg(test)] mod tests {{ /* {planted} */ }}\n"), 0),
            (
                format!("#[cfg(test)]\n/// {planted}\nfn helper() {{}}\n"),
                0,
            ),
            (format!("#[test]\nfn t() {{\n    {planted}\n}}\n"), 0),
            (
                format!("#[tokio::test]\nasync fn t() {{\n    {planted}\n}}\n"),
                0,
            ),
            (
                format!("#[cfg(all(test, feature = \"x\"))]\nmod t {{\n    {planted}\n}}\n"),
                0,
            ),
            (
                format!(
                    "struct S;\nimpl S {{\n    #[cfg(test)]\n    fn t(&self) {{\n        {planted}\n    }}\n}}\n"
                ),
                0,
            ),
            (
                format!(
                    "trait T {{\n    #[cfg(test)]\n    fn t(&self) {{\n        {planted}\n    }}\n}}\n"
                ),
                0,
            ),
            (
                format!(
                    "mod m {{\n    #[cfg(test)]\n    mod tests {{\n        {planted}\n    }}\n}}\n"
                ),
                0,
            ),
            // Production code that merely sits next to or around them.
            (
                format!("#[cfg(test)]\nmod tests {{}}\n{planted}\nfn f() {{}}\n"),
                1,
            ),
            (
                format!("fn f() {{}}\n{planted}\n#[cfg(test)]\nmod tests {{}}\n"),
                1,
            ),
            (
                format!("#[cfg(not(test))]\nmod prod {{\n    {planted}\n}}\n"),
                1,
            ),
            (
                format!("#[cfg(feature = \"x\")]\nmod feat {{\n    {planted}\n}}\n"),
                1,
            ),
            (
                format!("#[cfg(not(feature = \"x\"))]\nmod feat {{\n    {planted}\n}}\n"),
                1,
            ),
            (
                format!(
                    "#[cfg(not(any(test, feature = \"x\")))]\nmod feat {{\n    {planted}\n}}\n"
                ),
                1,
            ),
            (
                format!("#[cfg(test)]\nmod tests {{}}\nmod after {{\n    {planted}\n}}\n"),
                1,
            ),
            (
                format!(
                    "fn f() {{\n    {planted}\n}}\n#[cfg(test)]\nmod tests {{\n    {planted}\n}}\n"
                ),
                1,
            ),
        ] {
            let (_, hits) = scan(&rules, "scope.rs", &source);
            assert_eq!(hits.len(), expected, "{source}\n{hits:?}");
        }
    }

    #[test]
    fn spans_carry_positions() {
        // `proc-macro2/span-locations` is on for tests: with it off every
        // position is 0:0 and the test-only items would never be excluded.
        let file = syn::parse_file("fn f() {}\n#[cfg(test)]\nmod t {\n}\n").unwrap();
        assert_eq!(dropped_spans(&file), vec![((2, 0), (4, 1))]);
    }

    #[test]
    fn the_allowlist_matches_exactly_and_a_stale_entry_fails() {
        let hit = |file: &str, text: &str| Hit {
            file: file.to_owned(),
            line: 1,
            patterns: Vec::new(),
            text: text.to_owned(),
        };
        // The mechanism, on a fixture list (the real one is empty).
        let list: &[(&str, &str, &str)] = &[("a.rs", "review #1 here", "a reason")];
        let allowed = |h: &Hit| list.iter().any(|(f, t, _)| *f == h.file && *t == h.text);
        assert!(allowed(&hit("a.rs", "review #1 here")));
        assert!(!allowed(&hit("b.rs", "review #1 here")));
        assert!(!allowed(&hit("a.rs", "review #1 here too")));
        for (file, text, reason) in ALLOWED {
            assert!(!file.is_empty() && !text.is_empty() && reason.len() > 10);
        }
    }

    #[test]
    fn production_comments_state_invariants_not_review_history() {
        let rules = rules();
        let (files, blocks, hits) = scan_tree(&rules);
        // A floor, so a walk that finds nothing cannot pass.
        assert!(files >= 200, "only {files} production files read");
        assert!(blocks >= 3_500, "only {blocks} comments read");
        // Every allowlist entry must still match a comment.
        let hits_all: Vec<&Hit> = hits.iter().collect();
        for (file, text, reason) in ALLOWED {
            assert!(
                hits_all.iter().any(|h| h.file == *file && h.text == *text),
                "stale allowlist entry ({reason}): {file}: {text}"
            );
        }
        let refused: Vec<String> = hits
            .iter()
            .filter(|h| !is_allowed(h))
            .map(|h| {
                format!(
                    "{}:{}: {} [{}]",
                    h.file,
                    h.line,
                    h.text.chars().take(110).collect::<String>(),
                    h.patterns.join(" / ")
                )
            })
            .collect();
        assert!(
            refused.is_empty(),
            "{} comment(s) record review history; state the invariant instead (git history and the PR hold the rest):\n{}",
            refused.len(),
            refused.join("\n")
        );
    }
}
