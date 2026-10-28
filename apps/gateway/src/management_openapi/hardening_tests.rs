//! Adversarial controls for the shared slice-author gates.
use super::{quality, responses};
use serde_json::{Value, json};
use std::collections::BTreeSet;

fn closed(child: Value) -> Value {
    json!({"type":"object","required":["data"],"properties":{"data":child},"additionalProperties":false})
}
fn check(schema: Value) -> anyhow::Result<()> {
    quality::completed(&json!({"components":{"schemas":{"Probe":schema}},"paths":{}}))
}

#[test]
fn management_openapi_hardening_nested_array_and_map_escapes() {
    let bad = [
        json!({"type":"array"}),
        json!({"type":"array","items":true}),
        json!({"type":"array","items":{}}),
        json!({"type":"object","required":[],"additionalProperties":false,"patternProperties":{".*":{"type":"string"}}}),
        json!({"type":"object","required":[],"additionalProperties":{"type":"string"}}),
        json!({"type":"array","items":{"type":"string"},"prefixItems":[true]}),
        json!({"type":"array","items":{"type":"string"},"unevaluatedItems":true}),
        json!({"type":"array","items":{"type":"string"},"additionalItems":{}}),
        json!({"type":"array","contains":{"type":"string"}}),
        json!({"type":"array","items":{"type":"string"},"contains":true}),
    ];
    // Collect every escape so the RED run demonstrates each failure, not just
    // the first missing-items case.
    let mut escapes = Vec::new();
    for (i, child) in bad.into_iter().enumerate() {
        if check(closed(child.clone())).is_ok() {
            escapes.push(format!("nested {i}"));
        }
        for keyword in ["allOf", "anyOf", "oneOf", "if", "then", "else", "not"] {
            let branch = if ["allOf", "anyOf", "oneOf"].contains(&keyword) {
                json!({keyword:[child]})
            } else {
                json!({"type":"string",keyword:child})
            };
            if check(closed(branch)).is_ok() {
                escapes.push(format!("{keyword} {i}"));
            }
        }
        for kind in ["parameters", "headers"] {
            let doc =
                json!({"components":{"schemas":{},kind:{"Probe":{"schema":child}}},"paths":{}});
            if quality::completed(&doc).is_ok() {
                escapes.push(format!("{kind} {i}"));
            }
            let inline = if kind == "parameters" {
                json!({"parameters":[{"in":"query","name":"probe","schema":child}]})
            } else {
                json!({"responses":{"200":{"headers":{"Probe":{"schema":child}}}}})
            };
            if quality::operation(&doc, &inline).is_ok() {
                escapes.push(format!("inline {kind} {i}"));
            }
        }
    }
    assert!(escapes.is_empty(), "accepted escapes: {escapes:?}");
    for child in [
        json!({"type":"array","items":{"type":"string"}}),
        json!({"type":"array","items":closed(json!({"type":"integer"}))}),
        json!({"type":"array","prefixItems":[{"type":"string"}],"items":false}),
    ] {
        check(closed(child)).unwrap();
    }
}

#[test]
fn management_openapi_hardening_ceiling_details_identity_fields() {
    let schema = json!({"$ref":"#/components/schemas/IdentityAdminCeilingNotification"});
    let good = responses::review_tests::ceiling(true);
    responses::validate_value(responses::artifact(), &schema, &good).unwrap();
    let mut accepted = Vec::new();
    for (field, value) in [
        ("workspace_id", json!(uuid::Uuid::nil())),
        ("owner_email", json!("owner@private.invalid")),
        (
            "workspace",
            json!({"id":uuid::Uuid::nil(),"kind":"personal","name":"Private"}),
        ),
    ] {
        let mut leak = good.clone();
        leak["details"][field] = value;
        if responses::validate_value(responses::artifact(), &schema, &leak).is_ok() {
            accepted.push(field);
        }
    }
    assert!(
        accepted.is_empty(),
        "accepted details identities: {accepted:?}"
    );
}

#[test]
fn management_openapi_hardening_ceiling_producer_types_and_nullability() {
    let document = responses::artifact();
    let schema = &document["components"]["schemas"]["IdentityCeilingDetails"];
    let details = responses::review_tests::ceiling(true)["details"].clone();
    responses::validate_value(document, schema, &details).unwrap();
    for field in schema["required"].as_array().unwrap() {
        let field = field.as_str().unwrap();
        let mut omitted = details.clone();
        omitted.as_object_mut().unwrap().remove(field);
        assert!(
            responses::validate_value(document, schema, &omitted).is_err(),
            "missing producer field {field}"
        );
        let mut null = details.clone();
        null[field] = Value::Null;
        let nullable = [
            "threshold_requests_per_second",
            "lock_wait_p95_at_least_ms",
            "threshold_lock_wait_ms",
        ]
        .contains(&field);
        assert_eq!(
            responses::validate_value(document, schema, &null).is_ok(),
            nullable,
            "producer nullability {field}"
        );
    }
    for field in [
        "window_minutes",
        "admissions",
        "threshold_requests_per_second",
        "lock_wait_p95_at_least_ms",
        "threshold_lock_wait_ms",
    ] {
        let mut wrong = details.clone();
        wrong[field] = json!("200");
        assert!(
            responses::validate_value(document, schema, &wrong).is_err(),
            "stringified integer {field}"
        );
    }
}

fn bindings(sources: &[String]) -> BTreeSet<String> {
    use syn::visit::{self, Visit};
    #[derive(Default)]
    struct Finish(BTreeSet<String>);
    impl<'ast> Visit<'ast> for Finish {
        fn visit_expr_method_call(&mut self, call: &'ast syn::ExprMethodCall) {
            if call.method == "finish"
                && call.args.len() == 1
                && let syn::Expr::Path(path) = &call.args[0]
                && let Some(name) = path.path.segments.last()
            {
                self.0.insert(name.ident.to_string());
            }
            visit::visit_expr_method_call(self, call);
        }
        fn visit_item_fn(&mut self, _: &'ast syn::ItemFn) {} // no nested helpers
    }
    #[derive(Default)]
    struct Runners(BTreeSet<String>);
    impl<'ast> Visit<'ast> for Runners {
        fn visit_item_fn(&mut self, function: &'ast syn::ItemFn) {
            if function.attrs.iter().any(|a| {
                a.path()
                    .segments
                    .iter()
                    .map(|s| s.ident.to_string())
                    .collect::<Vec<_>>()
                    == ["sqlx", "test"]
            }) {
                let mut finish = Finish::default();
                finish.visit_block(&function.block);
                self.0.extend(finish.0);
            }
            visit::visit_item_fn(self, function);
        }
    }
    let mut runners = Runners::default();
    for source in sources {
        runners.visit_file(&syn::parse_file(source).unwrap());
    }
    runners.0
}

#[test]
fn management_openapi_hardening_area_groups_have_sqlx_finish_bindings() {
    fn sources(dir: &std::path::Path, out: &mut Vec<String>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                sources(&path, out);
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.push(std::fs::read_to_string(path).unwrap());
            }
        }
    }
    let mut all = Vec::new();
    sources(
        &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
        &mut all,
    );
    let bound = bindings(&all);
    // Parse the registry so a tuple's label must match its actual constant.
    let ast = syn::parse_file(include_str!("responses.rs")).unwrap();
    let registry = ast
        .items
        .iter()
        .find_map(|i| match i {
            syn::Item::Const(c) if c.ident == "AREA_GROUPS" => Some(c),
            _ => None,
        })
        .unwrap();
    let syn::Expr::Reference(reference) = &*registry.expr else {
        panic!("AREA_GROUPS must be a named array");
    };
    let syn::Expr::Array(array) = &*reference.expr else {
        panic!("AREA_GROUPS must be a named array");
    };
    let mut names = BTreeSet::new();
    for entry in &array.elems {
        let syn::Expr::Tuple(tuple) = entry else {
            panic!("AREA_GROUPS entry must be (constant name, constant)");
        };
        let [syn::Expr::Lit(label), syn::Expr::Path(group)] =
            tuple.elems.iter().collect::<Vec<_>>()[..]
        else {
            panic!("invalid group entry");
        };
        let syn::Lit::Str(label) = &label.lit else {
            panic!("group label must be a string");
        };
        let name = group.path.segments.last().unwrap().ident.to_string();
        assert_eq!(label.value(), name);
        assert!(names.insert(name.clone()), "duplicate group name {name}");
        assert!(
            bound.contains(&name),
            "{name} has no finish({name}) in a #[sqlx::test] runner"
        );
    }
}

#[test]
fn management_openapi_hardening_finish_inventory_ignores_claimed_helpers() {
    for source in [
        "fn helper() { observed.finish(MISSING); }",
        "#[test] fn unit() { observed.finish(MISSING); }",
        "#[sqlx::test] async fn runner() { /* observed.finish(MISSING) */ observed.finish(&[area]); }",
        "#[sqlx::test] async fn runner() { fn unused() { observed.finish(MISSING); } }",
    ] {
        assert!(!bindings(&[source.into()]).contains("MISSING"));
    }
    assert!(
        bindings(&[
            "#[sqlx::test] async fn runner() { observed.finish(responses::GROUP); }".into()
        ])
        .contains("GROUP")
    );
}
