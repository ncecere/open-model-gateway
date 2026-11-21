use super::*;

#[test]
fn additions_and_source_substitutions_change_the_inventory() {
    for (path, source, from, to) in [
        (
            "ops_log.rs",
            "enum OpsEvent { Demo { kind: Kind } }",
            "kind: Kind",
            "kind: Kind, mode: Mode",
        ),
        (
            "worker.rs",
            "fn run() { ops_log::emit(OpsEvent::Demo { kind: first }); }",
            "first",
            "second",
        ),
        (
            "ops_log.rs",
            r#"impl OpsEvent { fn attributes(&self) -> Labels { vec![("kind", self.first())] } }"#,
            "first",
            "second",
        ),
        (
            "ops_log.rs",
            r#"fn log_record() { record["traceId"] = json!(first); }"#,
            "first",
            "second",
        ),
        (
            "metrics.rs",
            r#"fn count() { family.get_or_create(&[("kind", first)]); }"#,
            "first)",
            "first), (\"mode\", second)",
        ),
        (
            "metrics.rs",
            r#"fn count() { family.get_or_create(&[("kind", first)]); }"#,
            "first",
            "second",
        ),
        (
            "metrics.rs",
            r#"fn count() { let labels = [("kind", first)]; family.get_or_create(&labels); }"#,
            "first",
            "second",
        ),
        (
            "metrics/registry.rs",
            r#"fn create() { series(&family, "kind", |i| first[i]); }"#,
            "first",
            "second",
        ),
        (
            "worker.rs",
            r#"fn run() { select! { _ = tick() => METRICS.observe_cache("one two", "ok") } }"#,
            "one two",
            "onetwo",
        ),
    ] {
        let before = inventory(&parse_fixture(path, source));
        let after = inventory(&parse_fixture(path, &source.replace(from, to)));
        assert_ne!(before, after, "{path}: {from} -> {to}");
    }
}

#[test]
fn test_only_producers_are_excluded() {
    let source = parse_fixture(
        "metrics.rs",
        r#"#[cfg(test)] fn count() { family.get_or_create(&[("kind", "demo")]); }"#,
    );
    assert!(inventory(&source).is_empty());
}

#[test]
fn producer_source_fixtures() {
    let cases = [
        (
            "worker.rs",
            "fn f(){ select! { () = tick() => { emit(event_one()); } } }",
            "event_one",
            "event_two",
        ),
        (
            "worker.rs",
            r#"fn f(){ select! { () = tick() => timer.phase("one") } }"#,
            "one",
            "two",
        ),
        (
            "metrics/collection.rs",
            r#"impl Metrics { fn f(&self){ self.collection_error("one"); } }"#,
            "one",
            "two",
        ),
        (
            "otlp.rs",
            r#"fixed_enum!(Delivery { Accepted => "one" });"#,
            "one",
            "two",
        ),
        (
            "telemetry.rs",
            r#"fixed_enum!(DropReason { Contexts => "contexts" });"#,
            "contexts",
            "context_limit",
        ),
        (
            "metrics_export.rs",
            r#"fixed_enum!(DropReason { Label => "one" });"#,
            "one",
            "two",
        ),
    ];

    for (path, source, from, to) in cases {
        let baseline = inventory(&parse_fixture(path, source));
        let changed = inventory(&parse_fixture(path, &source.replace(from, to)));
        assert_ne!(baseline, changed, "{path}: {source}");
        let unrelated = format!("fn unrelated() {{ ordinary({to:?}); }} {source}");
        assert_eq!(baseline, inventory(&parse_fixture(path, &unrelated)));
    }
}

#[test]
fn family_uses_resolve_bindings_at_the_lookup() {
    let lookup = "f.get_or_create(&provider);";
    let bound = "let provider = bounded(provider);";
    let check = |body: String| {
        let source = format!(
            "impl Metrics {{ fn set_batch_route_lines(&self) {{ for provider in rows {{ {body} }} }} }}"
        );
        let mut entries = inventory(&parse_fixture("metrics/jobs.rs", &source));
        entries.sort();
        entries
    };
    let macro_lookup = "select! { () = tick() => f.get_or_create(&provider) }";
    assert_ne!(
        check(format!("{bound} {macro_lookup}")),
        check(format!("{macro_lookup} {bound}"))
    );
    let bounded = check(format!("{bound} {lookup}"));
    assert_ne!(bounded, check(format!("{lookup} {bound}")));
    assert_ne!(bounded, check(format!("{{ {bound} }} {lookup}")));
    assert_eq!(bounded, check(format!("{{ {bound} {lookup} }}")));
    assert_ne!(
        check(format!("{bound} {lookup} {lookup}")),
        check(format!("{lookup} {bound} {lookup}"))
    );
}

#[test]
fn only_the_visible_source_changes_a_family_entry() {
    for (body, changed, hidden) in [
        (
            "let x = first(); f.get_or_create(&labels!(x)); let x = second();",
            "first",
            "second",
        ),
        (
            "let x = first(); { let x = second(); f.get_or_create(&x); }",
            "second",
            "first",
        ),
        (
            "let x = first(); if let Some(x) = second() {} else { f.get_or_create(&x); }",
            "first",
            "second",
        ),
        (
            "let x = first(); match second() { Some(x) => f.get_or_create(&x), None => () }",
            "second",
            "first",
        ),
        (
            "let x = first(); for x in second() {} f.get_or_create(&x);",
            "first",
            "second",
        ),
        (
            "let x = first(); let x = f.get_or_create(&x); let x = second();",
            "first",
            "second",
        ),
        (
            "let x = first(); let x = second().x; f.get_or_create(&x);",
            "second",
            "first",
        ),
    ] {
        let check = |body: &str| {
            inventory(&parse_fixture(
                "metrics.rs",
                &format!("fn count() {{ {body} }}"),
            ))
        };
        let before = check(body);
        assert_ne!(before, check(&body.replace(changed, "third")), "{body}");
        assert_eq!(before, check(&body.replace(hidden, "third")), "{body}");
    }
}
