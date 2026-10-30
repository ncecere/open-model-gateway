//! RED/GREEN controls for the contract-COVERAGE gates, using synthetic documents
//! and observations only (DB-free). Real observations come from the fixtures.
use super::{coverage::Observed, quality};
use serde_json::{Value, json};

const OP: &str = "GET /api/v1/x";

fn op(response: Value, parameters: Value) -> Value {
    json!({"parameters":parameters,"responses":{"200":{"description":"ok","content":{"application/json":{"schema":response}}}}})
}
fn doc(operation: Value, schemas: Value) -> Value {
    json!({"openapi":"3.1.0","paths":{"/api/v1/x":{"get":operation}},"components":{"schemas":schemas}})
}
fn run(document: &Value, bodies: &[Value], queries: &[&[&str]]) -> anyhow::Result<()> {
    let operation = &document["paths"]["/api/v1/x"]["get"];
    let mut observed = Observed::default();
    for names in queries {
        let names = names.iter().map(|n| n.to_string()).collect::<Vec<_>>();
        observed.observe_query(OP, &names);
    }
    for body in bodies {
        observed.observe_body(document, OP, operation, 200, body)?;
    }
    observed.check(document, OP, operation)
}
fn message(result: anyhow::Result<()>) -> String {
    format!("{:#}", result.expect_err("coverage gap must fail"))
}
fn query(name: &str) -> Value {
    json!({"in":"query","name":name,"schema":{"type":"string"}})
}

// 1. Query-parameter acceptance coverage.
#[test]
fn management_openapi_coverage_query_parameters_must_be_accepted_positively() {
    let document = doc(
        op(
            json!({"type":"object","required":[],"additionalProperties":false,"properties":{}}),
            json!([query("limit"), query("status")]),
        ),
        json!({}),
    );
    // RED: `status` is documented, but no positive request ever carried it.
    let gap = message(run(&document, &[json!({})], &[&["limit"]]));
    assert!(gap.contains("status") && !gap.contains("limit"), "{gap}");
    assert!(message(run(&document, &[json!({})], &[])).contains("limit"));
    // GREEN: accepted across separate observations.
    run(&document, &[json!({})], &[&["limit"], &["status", "limit"]]).unwrap();
    // Parameters are resolved through components as well.
    let mut by_ref = document.clone();
    by_ref["components"]["parameters"] = json!({"Status":query("status")});
    by_ref["paths"]["/api/v1/x"]["get"]["parameters"] =
        json!([{"$ref":"#/components/parameters/Status"}]);
    assert!(message(run(&by_ref, &[json!({})], &[&["limit"]])).contains("status"));
    run(&by_ref, &[json!({})], &[&["status"]]).unwrap();
    // Header/path/cookie parameters do not need a query observation.
    let mut other = document.clone();
    other["paths"]["/api/v1/x"]["get"]["parameters"] =
        json!([{"in":"header","name":"x-probe","schema":{"type":"string"}}]);
    run(&other, &[json!({})], &[]).unwrap();
}

fn nested() -> Value {
    json!({"type":"object","required":["data"],"additionalProperties":false,"properties":{
        "data":{"type":"array","items":{"type":"object","required":["outcomes"],"additionalProperties":false,
            "properties":{"outcomes":{"type":"array","items":{"type":"string"}}}}}}})
}

// 2. Nested population coverage.
#[test]
fn management_openapi_coverage_every_array_location_is_populated_once() {
    let document = doc(op(nested(), json!([])), json!({}));
    let empty_outer = json!({"data":[]});
    let empty_inner = json!({"data":[{"outcomes":[]}]});
    let full = json!({"data":[{"outcomes":["a"]}]});
    // RED: the outer array is empty, so the nested location was never seen.
    let gap = message(run(&document, std::slice::from_ref(&empty_outer), &[]));
    assert!(
        gap.contains("/data") && gap.contains("/data/*/outcomes"),
        "{gap}"
    );
    // RED: outer populated but the nested array was always empty.
    let gap = message(run(&document, std::slice::from_ref(&empty_inner), &[]));
    assert!(
        gap.contains("/data/*/outcomes") && !gap.contains("array /data "),
        "{gap}"
    );
    // GREEN: one populated observation is enough, even beside empty ones.
    run(&document, &[empty_outer, empty_inner, full.clone()], &[]).unwrap();
    // Indices are wildcarded: populating only the second element counts.
    run(
        &document,
        &[json!({"data":[{"outcomes":[]},{"outcomes":["b"]}]})],
        &[],
    )
    .unwrap();
    // Refs, allOf and nullable arrays are followed.
    let document = doc(
        op(json!({"$ref":"#/components/schemas/Root"}), json!([])),
        json!({"Root":{"allOf":[{"type":"object","required":["tags"],"additionalProperties":false,
            "properties":{"tags":{"type":["array","null"],"items":{"type":"string"}}}}]}}),
    );
    assert!(
        message(run(
            &document,
            &[json!({"tags":null}), json!({"tags":[]})],
            &[]
        ))
        .contains("/tags")
    );
    // Nullability is a branch too: the populated array alone is not enough.
    let gap = message(run(&document, &[json!({"tags":["x"]})], &[]));
    assert!(gap.contains("null") && gap.contains("/tags"), "{gap}");
    run(
        &document,
        &[json!({"tags":["x"]}), json!({"tags":null})],
        &[],
    )
    .unwrap();
}

#[test]
fn management_openapi_coverage_empty_array_exemption_needs_a_reason() {
    let mut schema = nested();
    schema["properties"]["data"]["items"]["properties"]["outcomes"]["x-omg-may-be-empty"] =
        json!("never populated: fixture databases have no outcomes");
    let document = doc(op(schema.clone(), json!([])), json!({}));
    // The exemption covers only the annotated location.
    let gap = message(run(&document, &[json!({"data":[]})], &[]));
    assert!(
        gap.contains("/data") && !gap.contains("/data/*/outcomes"),
        "{gap}"
    );
    run(&document, &[json!({"data":[{"outcomes":[]}]})], &[]).unwrap();
    // An empty reason is not an exemption (quality.rs also refuses it outright).
    schema["properties"]["data"]["items"]["properties"]["outcomes"]["x-omg-may-be-empty"] =
        json!("");
    let document = doc(op(schema, json!([])), json!({}));
    assert!(
        message(run(&document, &[json!({"data":[{"outcomes":[]}]})], &[])).contains("outcomes")
    );
}

fn unions() -> Value {
    json!({"type":"object","required":["note","kind"],"additionalProperties":false,"properties":{
        "note":{"anyOf":[{"type":"string"},{"type":"null"}]},
        "kind":{"oneOf":[{"$ref":"#/components/schemas/A"},{"$ref":"#/components/schemas/B"}]}}})
}
fn branches() -> Value {
    json!({
        "A":{"type":"object","required":["k"],"additionalProperties":false,"properties":{"k":{"const":"a"}}},
        "B":{"type":"object","required":["k","extra"],"additionalProperties":false,"properties":{"k":{"const":"b"},"extra":{"type":"integer"}}}
    })
}

// 3. Union-branch coverage.
#[test]
fn management_openapi_coverage_every_union_branch_matches_an_observed_value() {
    let document = doc(op(unions(), json!([])), branches());
    let a = json!({"note":"text","kind":{"k":"a"}});
    let b_null = json!({"note":null,"kind":{"k":"b","extra":1}});
    // RED: neither the null nor the B branch was ever the matching branch.
    let gap = message(run(&document, std::slice::from_ref(&a), &[]));
    assert!(gap.contains("/note") && gap.contains("/kind"), "{gap}");
    // RED: B and null observed, but the string branch of /note never matched.
    let gap = message(run(
        &document,
        &[json!({"note":null,"kind":{"k":"a"}}), b_null.clone()],
        &[],
    ));
    assert!(gap.contains("/note") && !gap.contains("/kind"), "{gap}");
    // GREEN: branches matched in different observations.
    run(&document, &[a.clone(), b_null.clone()], &[]).unwrap();
    // A value that matches no branch earns nothing and is not a coverage credit.
    assert!(
        run(
            &document,
            &[json!({"note":"t","kind":{"k":"z"}}), b_null.clone()],
            &[]
        )
        .is_err()
    );
}

#[test]
fn management_openapi_coverage_unobserved_branch_exemption_needs_a_reason() {
    let mut schema = unions();
    schema["properties"]["kind"]["oneOf"][1]["x-omg-unobserved-branch"] =
        json!("B needs a provider that the test database cannot configure");
    let document = doc(op(schema.clone(), json!([])), branches());
    let a = json!({"note":"text","kind":{"k":"a"}});
    let null_a = json!({"note":null,"kind":{"k":"a"}});
    run(&document, &[a.clone(), null_a.clone()], &[]).unwrap();
    // The exemption is for the one annotated branch only.
    assert!(message(run(&document, std::slice::from_ref(&a), &[])).contains("/note"));
    schema["properties"]["kind"]["oneOf"][1]["x-omg-unobserved-branch"] = json!("   ");
    let document = doc(op(schema, json!([])), branches());
    assert!(message(run(&document, &[a, null_a], &[])).contains("/kind"));
}

#[test]
fn management_openapi_coverage_unions_are_followed_through_every_composition() {
    // The union sits behind $ref, allOf, properties and items; the same union
    // component at two data locations must be covered at EACH location.
    let document = doc(
        op(
            json!({"type":"object","required":["first","rows"],"additionalProperties":false,"properties":{
                "first":{"allOf":[{"$ref":"#/components/schemas/Maybe"}]},
                "rows":{"type":"array","items":{"$ref":"#/components/schemas/Maybe"}}}}),
            json!([]),
        ),
        json!({"Maybe":{"anyOf":[{"type":"integer"},{"type":"null"}]}}),
    );
    let gap = message(run(
        &document,
        &[
            json!({"first":1,"rows":[1,null]}),
            json!({"first":2,"rows":[]}),
        ],
        &[],
    ));
    assert!(
        gap.contains("/first"),
        "the null branch at /first is unobserved: {gap}"
    );
    assert!(!gap.contains("/rows/*"), "{gap}");
    run(
        &document,
        &[
            json!({"first":null,"rows":[3,null]}),
            json!({"first":1,"rows":[1]}),
        ],
        &[],
    )
    .unwrap();
}

#[test]
fn management_openapi_coverage_conditionals_and_non_success_responses() {
    // `then`/`else` branches are reached; error responses are not walked.
    let mut document = doc(
        op(
            json!({"type":"object","required":["mode"],"additionalProperties":false,
                "properties":{"mode":{"enum":["a","b"]},"items":{"type":"array","items":{"type":"string"}}},
                "if":{"properties":{"mode":{"const":"a"}}},
                "then":{"required":["items"]},"else":{"not":{"required":["items"]}}}),
            json!([]),
        ),
        json!({}),
    );
    document["paths"]["/api/v1/x"]["get"]["responses"]["400"] = json!({"description":"bad","content":{"application/json":{"schema":{"type":"object","required":["list"],"additionalProperties":false,"properties":{"list":{"type":"array","items":{"type":"string"}}}}}}});
    assert!(message(run(&document, &[json!({"mode":"b"})], &[])).contains("/items"));
    run(&document, &[json!({"mode":"a","items":["x"]})], &[]).unwrap();
}

// Quality rules for the annotations themselves.
fn check(schema: Value) -> anyhow::Result<()> {
    quality::completed(&json!({"components":{"schemas":{"Probe":schema}},"paths":{}}))
}
fn closed(properties: Value) -> Value {
    let required = properties
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect::<Vec<_>>();
    json!({"type":"object","required":required,"additionalProperties":false,"properties":properties})
}

#[test]
fn management_openapi_coverage_annotation_reasons_are_validated() {
    let array = |reason: Value| {
        closed(
            json!({"list":{"type":"array","items":{"type":"string"},"x-omg-may-be-empty":reason}}),
        )
    };
    check(array(json!("reviewed reason"))).unwrap();
    for bad in [json!(""), json!("  "), json!(true), json!(null), json!(7)] {
        assert!(
            check(array(bad.clone())).is_err(),
            "accepted may-be-empty {bad}"
        );
    }
    // Only an array schema may claim to be allowed to be empty.
    assert!(
        check(closed(
            json!({"n":{"type":"string","x-omg-may-be-empty":"reason"}})
        ))
        .is_err()
    );
    let union = |reason: Value| {
        closed(
            json!({"u":{"anyOf":[{"type":"string","x-omg-unobserved-branch":reason},{"type":"null"}]}}),
        )
    };
    check(union(json!("reviewed reason"))).unwrap();
    for bad in [json!(""), json!(" "), json!(false), json!(null), json!(1)] {
        assert!(
            check(union(bad.clone())).is_err(),
            "accepted unobserved-branch {bad}"
        );
    }
    // Only a oneOf/anyOf branch may claim to be unobservable.
    assert!(
        check(closed(
            json!({"n":{"type":"string","x-omg-unobserved-branch":"reason"}})
        ))
        .is_err()
    );
    assert!(
        check(closed(
            json!({"n":{"allOf":[{"type":"string","x-omg-unobserved-branch":"reason"}]}})
        ))
        .is_err()
    );
}

// 4. Money format rule.
#[test]
fn management_openapi_coverage_microusd_properties_are_pattern_constrained_strings() {
    let amount = || json!({"type":"string","pattern":"^(0|[1-9][0-9]*)$"});
    let ok = [
        closed(json!({"cost_microusd":amount()})),
        closed(json!({"cost_microusd":{"type":["string","null"],"pattern":"^(0|-?[1-9][0-9]*)$"}})),
        closed(json!({"cost_microusd":{"anyOf":[amount(),{"type":"null"}]}})),
        closed(json!({"cost_microusd":{"oneOf":[{"type":"null"},amount()]}})),
        closed(json!({"cost_microusd":{"allOf":[amount()]}})),
        closed(json!({"cost_microusd":{"type":"null"}})),
        closed(json!({"cost_microusd":{"const":null}})),
    ];
    for schema in ok {
        check(schema.clone()).unwrap_or_else(|e| panic!("rejected {schema}: {e:#}"));
    }
    let bad = [
        json!({"type":"string"}),
        json!({"type":["string","null"]}),
        json!({"type":"integer"}),
        json!({"type":"number"}),
        json!({"type":"string","minLength":1,"maxLength":30}),
        json!({"type":"string","pattern":".*"}),
        json!({"type":"string","pattern":"^.+$"}),
        json!({"type":"string","pattern":"^[0-9.]+$"}),
        // Leading zeros, `-0` and signs the unsigned form must not admit.
        json!({"type":"string","pattern":"^[0-9]+$"}),
        json!({"type":"string","pattern":"^(0|[1-9][0-9]*)"}),
        json!({"type":"string","pattern":"^-?(0|[1-9][0-9]*)$"}),
        json!({"type":"string","pattern":"^-?[0-9]+$"}),
        json!({"type":"string","pattern":"^(0|-?[1-9][0-9]*)$|^-0$"}),
        json!({"type":"string","pattern":"^[+-]?(0|[1-9][0-9]*)$"}),
        json!({"type":"string","pattern":"^(0|[1-9][0-9]*)\\.?[0-9]*$"}),
        json!({"type":"string","format":"uint64"}),
        json!({"type":"string","enum":["0","1"]}),
        json!({"anyOf":[{"type":"string"},{"type":"null"}]}),
        json!({"anyOf":[amount(),{"type":"integer"}]}),
        json!({"allOf":[{"type":"string"}]}),
        json!({"$ref":"#/components/schemas/Loose"}),
    ];
    for property in bad {
        let document = json!({"components":{"schemas":{
            "Probe":closed(json!({"held_microusd":property})),
            "Loose":{"type":"string"}}},"paths":{}});
        assert!(
            quality::completed(&document).is_err(),
            "accepted money property {property}"
        );
    }
    // A reviewed component with a real pattern may be referenced, also nullable.
    let document = json!({"components":{"schemas":{
        "Probe":closed(json!({"a_microusd":{"$ref":"#/components/schemas/Money"},"b_microusd":{"anyOf":[{"$ref":"#/components/schemas/Money"},{"type":"null"}]}})),
        "Money":amount()}},"paths":{}});
    quality::completed(&document).unwrap();
    // Request schemas, nested objects, array items and conditional branches too.
    for wrap in [
        |p: Value| closed(json!({"outer":closed(json!({"x_microusd":p}))})),
        |p: Value| closed(json!({"rows":{"type":"array","items":closed(json!({"x_microusd":p}))}})),
        |p: Value| json!({"allOf":[closed(json!({"x_microusd":p}))]}),
        |p: Value| json!({"oneOf":[closed(json!({"x_microusd":p})),{"type":"null"}]}),
    ] {
        assert!(check(wrap(json!({"type":"string"}))).is_err());
        check(wrap(amount())).unwrap();
    }
    // Only names ending in _microusd are money; others are unconstrained here.
    check(closed(
        json!({"microusd_note":{"type":"string"},"price_micro_usd":{"type":"string"}}),
    ))
    .unwrap();
}

// 5. ARBITRARY_JSON nodes declare a type.
#[test]
fn management_openapi_coverage_arbitrary_json_nodes_declare_a_type() {
    let (pointer, reason) = quality::ARBITRARY_JSON[0];
    assert_eq!(
        pointer,
        "/components/schemas/IdentityNotification/properties/details"
    );
    let node = |ty: Option<Value>| {
        let mut node = json!({"$comment":reason,"patternProperties":{"(^|_)microusd$":{"type":"string","pattern":"^(0|[1-9][0-9]*)$"}}});
        if let Some(ty) = ty {
            node["type"] = ty;
        }
        json!({"components":{"schemas":{"IdentityNotification":closed(json!({"details":node}))}},"paths":{}})
    };
    quality::completed(&node(Some(json!("object")))).unwrap();
    quality::completed(&node(Some(json!(["object", "null"])))).unwrap();
    assert!(
        quality::completed(&node(None)).is_err(),
        "untyped arbitrary JSON accepted"
    );
    assert!(quality::completed(&node(Some(json!([])))).is_err());
    assert!(quality::completed(&node(Some(json!(7)))).is_err());
    // The committed artifact's reviewed nodes all declare a type.
    for (pointer, _) in quality::ARBITRARY_JSON {
        let node = super::responses::artifact().pointer(pointer).unwrap();
        assert!(
            node["type"].is_string() || node["type"].is_array(),
            "{pointer}"
        );
    }
}

// 3b. Nullability is a branch: null AND the non-null form must be observed.
#[test]
fn management_openapi_coverage_nullability_is_a_branch() {
    let nullable = |ty: Value| {
        doc(
            op(
                json!({"type":"object","required":["cost"],"additionalProperties":false,"properties":{"cost":{"type":ty}}}),
                json!([]),
            ),
            json!({}),
        )
    };
    let document = nullable(json!(["string", "null"]));
    // RED: a field documented nullable that was only ever populated (the
    // handler may coalesce unknown to a value) or only ever null.
    let gap = message(run(
        &document,
        &[json!({"cost":"1"}), json!({"cost":"2"})],
        &[],
    ));
    assert!(gap.contains("/cost") && gap.contains("null"), "{gap}");
    let gap = message(run(&document, &[json!({"cost":null})], &[]));
    assert!(gap.contains("/cost") && gap.contains("non-null"), "{gap}");
    run(&document, &[json!({"cost":"1"}), json!({"cost":null})], &[]).unwrap();
    // An explicit reviewed exemption, with a reason, on the nullable node.
    let mut exempt = document.clone();
    exempt["paths"]["/api/v1/x"]["get"]["responses"]["200"]["content"]["application/json"]["schema"]
        ["properties"]["cost"]["x-omg-unobserved-branch"] =
        json!("null: needs a pricing gap the fixtures cannot create");
    run(&exempt, &[json!({"cost":"1"})], &[]).unwrap();
    exempt["paths"]["/api/v1/x"]["get"]["responses"]["200"]["content"]["application/json"]["schema"]
        ["properties"]["cost"]["x-omg-unobserved-branch"] = json!(" ");
    assert!(run(&exempt, &[json!({"cost":"1"})], &[]).is_err());
    // Enums that list null are nullable too.
    let document = doc(
        op(
            json!({"type":"object","required":["mode"],"additionalProperties":false,"properties":{"mode":{"enum":["a",null]}}}),
            json!([]),
        ),
        json!({}),
    );
    assert!(message(run(&document, &[json!({"mode":"a"})], &[])).contains("/mode"));
    run(&document, &[json!({"mode":"a"}), json!({"mode":null})], &[]).unwrap();
    // A union with a {"type":"null"} branch, and $refs to nullable components,
    // are each covered at every data location they appear in.
    let document = doc(
        op(
            json!({"type":"object","required":["first","rows"],"additionalProperties":false,"properties":{
                "first":{"$ref":"#/components/schemas/Cost"},
                "rows":{"type":"array","items":{"type":"object","required":["cost","other"],"additionalProperties":false,
                    "properties":{"cost":{"$ref":"#/components/schemas/Cost"},"other":{"anyOf":[{"type":"integer"},{"type":"null"}]}}}}}}),
            json!([]),
        ),
        json!({"Cost":{"type":["string","null"]}}),
    );
    let gap = message(run(
        &document,
        &[
            json!({"first":"1","rows":[{"cost":null,"other":1}]}),
            json!({"first":"2","rows":[{"cost":"3","other":null}]}),
        ],
        &[],
    ));
    assert!(
        gap.contains("`/first`") && gap.contains("null") && !gap.contains("/rows/*"),
        "{gap}"
    );
    run(
        &document,
        &[
            json!({"first":null,"rows":[{"cost":null,"other":1}]}),
            json!({"first":"2","rows":[{"cost":"3","other":null}]}),
        ],
        &[],
    )
    .unwrap();
    // A non-nullable type list is not a nullability branch.
    let document = doc(
        op(
            json!({"type":"object","required":["n"],"additionalProperties":false,"properties":{"n":{"type":["integer","number"]}}}),
            json!([]),
        ),
        json!({}),
    );
    run(&document, &[json!({"n":1})], &[]).unwrap();
}

#[test]
fn management_openapi_coverage_nullable_exemption_is_validated_by_quality() {
    let node = |reason: Value| {
        closed(json!({"n":{"type":["string","null"],"x-omg-unobserved-branch":reason}}))
    };
    check(node(json!("null: reviewed reason"))).unwrap();
    check(node(json!("non-null: reviewed reason"))).unwrap();
    for bad in [
        json!(""),
        json!("  "),
        json!(true),
        json!(3),
        // The side is mandatory, and so is the reason after it.
        json!("reviewed reason"),
        json!("null:"),
        json!("non-null:  "),
        json!("Null: reason"),
        json!("both: reason"),
    ] {
        assert!(check(node(bad.clone())).is_err(), "accepted {bad}");
    }
    check(closed(
        json!({"n":{"enum":["a",null],"x-omg-unobserved-branch":"null: reviewed reason"}}),
    ))
    .unwrap();
    // A non-nullable node still cannot claim an unobserved branch.
    assert!(
        check(closed(
            json!({"n":{"type":["string","integer"],"x-omg-unobserved-branch":"reason"}})
        ))
        .is_err()
    );
}

// 4b. Money patterns reject leading zeros and `-0`; decimal rates are listed.
#[test]
fn management_openapi_coverage_decimal_rate_exemptions_are_reviewed() {
    let loose = json!({"type":"string","pattern":"^[0-9]+(\\.[0-9]+)?$"});
    for name in [
        "conversion_residual_microusd",
        "input_microusd_per_million",
        "cache_read_microusd_per_million",
    ] {
        check(closed(json!({name:loose}))).unwrap_or_else(|e| panic!("{name}: {e:#}"));
    }
    for name in ["rate_microusd", "residual_microusd_extra", "price_microusd"] {
        assert!(
            check(closed(json!({name:loose}))).is_err(),
            "{name} is not a reviewed rate"
        );
    }
    for (name, justification) in quality::DECIMAL_RATES {
        assert!(
            !name.is_empty() && justification.trim().len() > 20,
            "{name}"
        );
    }
}

// 5. A query parameter that can never ride a positive request (the handler
// always refuses a request that carries it) is exempt ONLY with a reason.
#[test]
fn management_openapi_coverage_unobservable_query_parameter_needs_a_reason() {
    let exempt = |reason: Value| {
        let mut parameter = query("error");
        parameter["x-omg-unobserved-parameter"] = reason;
        doc(
            op(
                json!({"type":"object","required":[],"additionalProperties":false,"properties":{}}),
                json!([query("state"), parameter]),
            ),
            json!({}),
        )
    };
    let document = exempt(json!("the callback always refuses an `error` response"));
    // Exempt for `error` only: the other parameter is still required.
    let gap = message(run(&document, &[json!({})], &[]));
    assert!(gap.contains("state") && !gap.contains("error"), "{gap}");
    run(&document, &[json!({})], &[&["state"]]).unwrap();
    // An empty reason excuses nothing, and quality.rs refuses it outright.
    for bad in [json!(""), json!("  "), json!(true)] {
        let document = exempt(bad.clone());
        assert!(
            message(run(&document, &[json!({})], &[&["state"]])).contains("error"),
            "{bad}"
        );
        let operation = &document["paths"]["/api/v1/x"]["get"];
        assert!(quality::operation(&document, operation).is_err(), "{bad}");
    }
    // Only a query parameter may claim it.
    let mut document = exempt(json!("reviewed reason"));
    document["paths"]["/api/v1/x"]["get"]["parameters"] = json!([{
        "in":"header","name":"x-probe","schema":{"type":"string"},
        "x-omg-unobserved-parameter":"reviewed reason"}]);
    let operation = &document["paths"]["/api/v1/x"]["get"];
    assert!(quality::operation(&document, operation).is_err());
}

// ----- coverage engine round 2 ----------------------------------------------

#[test]
fn management_openapi_coverage_a_stale_unobserved_parameter_exemption_is_refused() {
    let mut parameter = query("error");
    parameter["x-omg-unobserved-parameter"] = json!("the handler always refuses `error`");
    let document = doc(
        op(
            json!({"type":"object","required":[],"additionalProperties":false,"properties":{}}),
            json!([query("state"), parameter]),
        ),
        json!({}),
    );
    run(&document, &[json!({})], &[&["state"]]).unwrap();
    // The exempt parameter WAS accepted by a positive request: the exemption lies.
    let gap = message(run(&document, &[json!({})], &[&["state", "error"]]));
    assert!(
        gap.contains("error") && gap.contains("stale") && !gap.contains("`state`"),
        "{gap}"
    );
    // Across separate positive observations too.
    assert!(message(run(&document, &[json!({})], &[&["state"], &["error"]])).contains("stale"));
}

#[test]
fn management_openapi_coverage_a_schema_reference_cycle_fails_cleanly() {
    let document = doc(
        op(json!({"$ref":"#/components/schemas/A"}), json!([])),
        json!({"A":{"allOf":[{"$ref":"#/components/schemas/B"}]},
               "B":{"allOf":[{"$ref":"#/components/schemas/A"}]}}),
    );
    let text = message(run(&document, &[json!({})], &[]));
    assert!(
        text.contains("cycle") && text.contains("/components/schemas/"),
        "{text}"
    );
    // A self-reference with no data progress, too.
    let document = doc(
        op(json!({"$ref":"#/components/schemas/Loop"}), json!([])),
        json!({"Loop":{"$ref":"#/components/schemas/Loop"}}),
    );
    assert!(message(run(&document, &[json!({})], &[])).contains("cycle"));
    // Legitimate recursion consumes data and is not a cycle.
    let tree = doc(
        op(json!({"$ref":"#/components/schemas/Node"}), json!([])),
        json!({"Node":{"type":"object","required":["children"],"additionalProperties":false,
            "properties":{"children":{"type":"array","items":{"$ref":"#/components/schemas/Node"}}}}}),
    );
    run(
        &tree,
        &[json!({"children":[{"children":[{"children":[]}]}]})],
        &[],
    )
    .unwrap();
}

fn typed_map() -> Value {
    json!({"type":"object","required":["details"],"additionalProperties":false,"properties":{
        "details":{"type":"object",
            "patternProperties":{"_microusd$":{"anyOf":[{"type":"string"},{"type":"null"}]}}}}})
}

#[test]
fn management_openapi_coverage_typed_map_values_need_their_branches_observed() {
    let document = doc(op(typed_map(), json!([])), json!({}));
    let text_only = json!({"details":{"a_microusd":"1","b_microusd":"2"}});
    let null_only = json!({"details":{"a_microusd":null}});
    // RED: the null branch of the values under /details/* never matched.
    let gap = message(run(&document, std::slice::from_ref(&text_only), &[]));
    assert!(gap.contains("/details/*") && gap.contains("null"), "{gap}");
    let gap = message(run(&document, std::slice::from_ref(&null_only), &[]));
    assert!(
        gap.contains("/details/*") && gap.contains("string"),
        "{gap}"
    );
    // A name the pattern does not select earns nothing.
    let gap = message(run(
        &document,
        &[json!({"details":{"other":null,"a_microusd":"1"}})],
        &[],
    ));
    assert!(gap.contains("/details/*"), "{gap}");
    // GREEN: both branches, in different observations and across keys.
    run(&document, &[text_only.clone(), null_only.clone()], &[]).unwrap();
    run(
        &document,
        &[json!({"details":{"a_microusd":"1","b_microusd":null}})],
        &[],
    )
    .unwrap();
    // additionalProperties values are walked at the same wildcard location,
    // and arrays under them must be populated.
    let additional = doc(
        op(
            json!({"type":"object","required":["m"],"additionalProperties":false,"properties":{
                "m":{"type":"object","additionalProperties":{"type":"array","items":{"type":"string"}}}}}),
            json!([]),
        ),
        json!({}),
    );
    let gap = message(run(&additional, &[json!({"m":{"k":[]}})], &[]));
    assert!(gap.contains("/m/*"), "{gap}");
    run(&additional, &[json!({"m":{"k":[],"j":["x"]}})], &[]).unwrap();
    // additionalProperties applies only to names no other keyword selects.
    let mixed = doc(
        op(
            json!({"type":"object","required":["m"],"additionalProperties":false,"properties":{
                "m":{"type":"object","properties":{"fixed":{"type":["integer","null"]}},
                    "additionalProperties":{"type":["integer","null"]}}}}),
            json!([]),
        ),
        json!({}),
    );
    let gap = message(run(&mixed, &[json!({"m":{"fixed":null,"a":1}})], &[]));
    assert!(gap.contains("`/m/*`") && gap.contains("null"), "{gap}");
    run(
        &mixed,
        &[
            json!({"m":{"fixed":null,"a":1}}),
            json!({"m":{"fixed":3,"b":null}}),
        ],
        &[],
    )
    .unwrap();
}

#[test]
fn management_openapi_coverage_nullable_exemptions_name_the_side_they_exempt() {
    let nullable = |reason: Option<&str>| {
        let mut cost = json!({"type":["string","null"]});
        if let Some(reason) = reason {
            cost["x-omg-unobserved-branch"] = json!(reason);
        }
        doc(
            op(
                json!({"type":"object","required":["cost"],"additionalProperties":false,"properties":{"cost":cost}}),
                json!([]),
            ),
            json!({}),
        )
    };
    let null = json!({"cost":null});
    let some = json!({"cost":"1"});
    let null_exempt = nullable(Some("null: the fixtures cannot create a pricing gap"));
    run(&null_exempt, std::slice::from_ref(&some), &[]).unwrap();
    // `null:` exempts the null side only: the non-null side is still required.
    let gap = message(run(&null_exempt, std::slice::from_ref(&null), &[]));
    assert!(gap.contains("/cost") && gap.contains("non-null"), "{gap}");
    let non_null_exempt = nullable(Some("non-null: cost is always unknown in fixtures"));
    run(&non_null_exempt, std::slice::from_ref(&null), &[]).unwrap();
    let gap = message(run(&non_null_exempt, std::slice::from_ref(&some), &[]));
    // ...and the exempt non-null side WAS observed, so that exemption is stale too.
    assert!(
        gap.contains("/cost")
            && gap.contains("branch 0 `null` never matched")
            && gap.contains("branch 1 `non-null` carries")
            && gap.contains("stale"),
        "{gap}"
    );
    // A reason with no side exempts neither.
    let sideless = nullable(Some("cost is sometimes unknown"));
    assert!(message(run(&sideless, std::slice::from_ref(&some), &[])).contains("null"));
    assert!(message(run(&sideless, std::slice::from_ref(&null), &[])).contains("non-null"));
}

#[test]
fn management_openapi_coverage_exemptions_are_forbidden_on_shared_component_targets() {
    // An annotation under a component applies at every place that references
    // the component, so it is refused when more than one place does (at any
    // depth); referenced once, it is as inline as an annotation can be.
    let probe = |component: Value, refs: usize| {
        let properties: serde_json::Map<String, Value> = (0..refs)
            .map(|i| {
                (
                    format!("p{i}"),
                    json!({"$ref":"#/components/schemas/Shared"}),
                )
            })
            .collect();
        json!({"components":{"schemas":{"Shared":component,
            "Probe":closed(Value::Object(properties))}},"paths":{}})
    };
    let array = |nested: bool| {
        let list = json!({"type":"array","items":{"type":"string"},"x-omg-may-be-empty":"reviewed reason"});
        if nested {
            closed(json!({"inner":list}))
        } else {
            list
        }
    };
    let text = |document: &Value| format!("{:#}", quality::completed(document).unwrap_err());
    quality::completed(&probe(json!({"type":"array","items":{"type":"string"}}), 2)).unwrap();
    for nested in [false, true] {
        // Referenced once: allowed, at the top of the component and deep inside it.
        quality::completed(&probe(array(nested), 1)).unwrap();
        // Referenced twice: refused at any depth.
        let message = text(&probe(array(nested), 2));
        assert!(message.contains("shared component"), "{message}");
    }
    quality::completed(&probe(
        json!({"type":["string","null"],
        "x-omg-unobserved-branch":"null: reviewed reason"}),
        1,
    ))
    .unwrap();
    assert!(
        text(&probe(
            json!({"type":["string","null"],
        "x-omg-unobserved-branch":"null: reviewed reason"}),
            2
        ))
        .contains("shared component")
    );
    // A branch annotation deep inside a shared object component.
    let branch = closed(json!({"u":{"anyOf":[
        {"type":"string","x-omg-unobserved-branch":"reviewed reason"},{"type":"null"}]}}));
    quality::completed(&probe(branch.clone(), 1)).unwrap();
    assert!(text(&probe(branch, 2)).contains("shared component"));
    // The sharing is transitive: a component referenced ONCE, by a component
    // that is referenced twice, applies at both places.
    let transitive = |outer_refs: usize| {
        let properties: serde_json::Map<String, Value> = (0..outer_refs)
            .map(|i| {
                (
                    format!("p{i}"),
                    json!({"$ref":"#/components/schemas/Outer"}),
                )
            })
            .collect();
        json!({"components":{"schemas":{
            "Inner":array(false),
            "Outer":closed(json!({"inner":{"$ref":"#/components/schemas/Inner"}})),
            "Probe":closed(Value::Object(properties))}},"paths":{}})
    };
    quality::completed(&transitive(1)).unwrap();
    assert!(text(&transitive(2)).contains("shared component"));
    // The same annotation inline is fine, and so is a nullable union branch rule.
    check(closed(
        json!({"list":{"type":"array","items":{"type":"string"},
        "x-omg-may-be-empty":"reviewed reason"}}),
    ))
    .unwrap();
    // A nullable union branch is ambiguous: split the null into its own branch.
    assert!(
        check(closed(json!({"u":{"anyOf":[
        {"type":["string","null"],"x-omg-unobserved-branch":"null: reason"},{"type":"integer"}]}})))
        .is_err()
    );
    // A `$ref` with the annotation beside it would be silently ignored: refused.
    assert!(
        quality::completed(&json!({"components":{"schemas":{
        "Maybe":{"type":["string","null"]},
        "Probe":closed(json!({"n":{"$ref":"#/components/schemas/Maybe",
            "x-omg-unobserved-branch":"null: reason"}}))}},"paths":{}}))
        .is_err()
    );
}

#[test]
fn management_openapi_coverage_byte_limits_are_reviewed() {
    let string = |extra: Value| {
        let mut node = json!({"type":"string"});
        for (k, v) in extra.as_object().unwrap() {
            node[k] = v.clone();
        }
        closed(json!({"code":node}))
    };
    check(string(json!({"maxLength":8,"x-omg-max-bytes":8}))).unwrap();
    check(string(json!({"maxLength":4,"x-omg-max-bytes":8}))).unwrap();
    // maxLength (characters) can never exceed the byte limit.
    assert!(check(string(json!({"maxLength":9,"x-omg-max-bytes":8}))).is_err());
    for bad in [
        json!(0),
        json!(-1),
        json!("8"),
        json!(1.5),
        json!(null),
        json!(true),
    ] {
        assert!(
            check(string(json!({"x-omg-max-bytes":bad.clone()}))).is_err(),
            "accepted {bad}"
        );
    }
    // Only a string schema has bytes.
    assert!(check(closed(json!({"n":{"type":"integer","x-omg-max-bytes":8}}))).is_err());
}

#[test]
fn management_openapi_coverage_stale_array_and_branch_exemptions_are_refused() {
    let may_be_empty = doc(
        op(
            closed(json!({"list":{"type":"array","items":{"type":"string"},
                "x-omg-may-be-empty":"The list is always empty for this fixture."}})),
            json!([]),
        ),
        json!({}),
    );
    // GREEN: the array was never non-empty, so the exemption is still needed.
    run(&may_be_empty, &[json!({"list":[]})], &[]).unwrap();
    // RED: it WAS non-empty, so the exemption is stale.
    let gap = message(run(&may_be_empty, &[json!({"list":["a"]})], &[]));
    assert!(
        gap.contains("/list") && gap.contains("x-omg-may-be-empty") && gap.contains("stale"),
        "{gap}"
    );
    let branch = |reason: &str| {
        doc(
            op(
                closed(json!({"u":{"anyOf":[
                    {"type":"string","x-omg-unobserved-branch":reason},{"type":"integer"}]}})),
                json!([]),
            ),
            json!({}),
        )
    };
    let document = branch("Strings never occur here.");
    run(&document, &[json!({"u":1})], &[]).unwrap();
    let gap = message(run(&document, &[json!({"u":1}), json!({"u":"a"})], &[]));
    assert!(
        gap.contains("branch 0")
            && gap.contains("x-omg-unobserved-branch")
            && gap.contains("stale"),
        "{gap}"
    );
    // A nullable side exempted by `null:` is stale once null WAS observed, and
    // `non-null:` once a value was.
    let nullable = |reason: &str| {
        doc(
            op(
                closed(json!({"n":{"type":["string","null"],"x-omg-unobserved-branch":reason}})),
                json!([]),
            ),
            json!({}),
        )
    };
    let null_exempt = nullable("null: the value is never unknown.");
    run(&null_exempt, &[json!({"n":"a"})], &[]).unwrap();
    let gap = message(run(
        &null_exempt,
        &[json!({"n":"a"}), json!({"n":null})],
        &[],
    ));
    assert!(gap.contains("stale") && gap.contains("null"), "{gap}");
    let value_exempt = nullable("non-null: the value is always unknown.");
    run(&value_exempt, &[json!({"n":null})], &[]).unwrap();
    let gap = message(run(
        &value_exempt,
        &[json!({"n":null}), json!({"n":"a"})],
        &[],
    ));
    assert!(gap.contains("stale") && gap.contains("non-null"), "{gap}");
}

/// A document whose only operation takes `request` (a schema) as its JSON body.
fn request_document(components: Value, request: Value) -> Value {
    json!({"components":{"schemas":components},"paths":{"/x":{"post":{"requestBody":{
        "required":true,"content":{"application/json":{"schema":request}}}}}}})
}

#[test]
fn management_openapi_coverage_vector_annotations_are_validated() {
    let reason = "Reviewed: the handler caps this beyond one request.";
    let request = |property: Value| {
        request_document(
            json!({"Req":closed(json!({"a":property}))}),
            json!({"$ref":"#/components/schemas/Req"}),
        )
    };
    let string = |extra: Value| {
        let mut node = json!({"type":"string","pattern":"^(a|b)+$"});
        for (k, v) in extra.as_object().unwrap() {
            node[k] = v.clone();
        }
        node
    };
    // x-omg-unsendable-boundary: a direct property of a request body, with a reason.
    quality::completed(&request(string(
        json!({"x-omg-unsendable-boundary":reason}),
    )))
    .unwrap();
    for bad in [json!(""), json!("  "), json!(true), json!(null)] {
        assert!(
            quality::completed(&request(string(json!({"x-omg-unsendable-boundary":bad})))).is_err()
        );
    }
    // ...not a nested property, not a response-only schema, not an arbitrary node.
    let nested = request(
        json!({"type":"object","required":["b"],"additionalProperties":false,
        "properties":{"b":string(json!({"x-omg-unsendable-boundary":reason}))}}),
    );
    let text = format!("{:#}", quality::completed(&nested).unwrap_err());
    assert!(
        text.contains("x-omg-unsendable-boundary") && text.contains("direct"),
        "{text}"
    );
    let response_only = json!({"components":{"schemas":{
        "Res":closed(json!({"a":string(json!({"x-omg-unsendable-boundary":reason}))}))}},
        "paths":{"/x":{"get":{"responses":{"200":{"description":"ok","content":{
            "application/json":{"schema":{"$ref":"#/components/schemas/Res"}}}}}}}}});
    assert!(quality::completed(&response_only).is_err());
    // A union arm of the request root counts as direct.
    let arms = request_document(
        json!({"Req":{"oneOf":[closed(json!({"a":string(json!({"x-omg-unsendable-boundary":reason}))}))]}}),
        json!({"$ref":"#/components/schemas/Req"}),
    );
    quality::completed(&arms).unwrap();
    // On a query parameter it sits on the parameter object.
    let operation = |parameter: Value| json!({"parameters":[parameter],"responses":{}});
    let parameter = |location: &str, reason: Value| {
        json!({"in":location,"name":"q","schema":{"type":"string"},
            "x-omg-unsendable-boundary":reason})
    };
    quality::operation(&json!({}), &operation(parameter("query", json!(reason)))).unwrap();
    assert!(
        quality::operation(&json!({}), &operation(parameter("header", json!(reason)))).is_err()
    );
    assert!(quality::operation(&json!({}), &operation(parameter("query", json!("")))).is_err());

    // x-omg-unprobed-location: inside a request body schema only, with a reason.
    quality::completed(&request(string(json!({"x-omg-unprobed-location":reason})))).unwrap();
    assert!(quality::completed(&request(string(json!({"x-omg-unprobed-location":""})))).is_err());
    assert!(quality::completed(&request(string(json!({"x-omg-unprobed-location":7})))).is_err());
    assert!(quality::completed(&response_only_unprobed(reason)).is_err());
    // x-omg-pattern-boundaries: a string schema with a pattern, with a reason.
    quality::completed(&request(string(json!({"x-omg-pattern-boundaries":reason})))).unwrap();
    assert!(quality::completed(&request(string(json!({"x-omg-pattern-boundaries":" "})))).is_err());
    assert!(
        quality::completed(&request(
            json!({"type":"string","x-omg-pattern-boundaries":reason})
        ))
        .is_err()
    );
    // Every one is refused under a shared component, whatever its other rules.
    for annotation in [
        "x-omg-unprobed-location",
        "x-omg-unsendable-boundary",
        "x-omg-pattern-boundaries",
    ] {
        let shared = request_document(
            json!({"Shared":closed(json!({"a":string(json!({annotation:reason}))})),
                "Req":closed(json!({"x":{"$ref":"#/components/schemas/Shared"},
                    "y":{"$ref":"#/components/schemas/Shared"}}))}),
            json!({"$ref":"#/components/schemas/Req"}),
        );
        let text = format!("{:#}", quality::completed(&shared).unwrap_err());
        assert!(text.contains("shared component"), "{annotation}: {text}");
    }
}
fn response_only_unprobed(reason: &str) -> Value {
    json!({"components":{"schemas":{"Res":closed(json!({"a":{"type":"string",
        "x-omg-unprobed-location":reason}}))}},
        "paths":{"/x":{"get":{"responses":{"200":{"description":"ok","content":{
            "application/json":{"schema":{"$ref":"#/components/schemas/Res"}}}}}}}}})
}

#[test]
fn management_openapi_coverage_byte_limit_placement_is_refused_in_branches_and_conditionals() {
    let limited = || json!({"type":"string","x-omg-max-bytes":8});
    // Always-applicable placements.
    check(closed(json!({"code":limited()}))).unwrap();
    check(closed(json!({"code":{"allOf":[limited()]}}))).unwrap();
    // A property INSIDE a branch is its own instance.
    check(closed(json!({"u":{"oneOf":[
        closed(json!({"code":limited()})),{"type":"null"}]}})))
    .unwrap();
    // A union branch that IS the limited string: the limit would hold only when it matches.
    for keyword in ["oneOf", "anyOf"] {
        let text = format!(
            "{:#}",
            check(closed(
                json!({"code":{keyword:[limited(),{"type":"null"}]}})
            ))
            .unwrap_err()
        );
        assert!(
            text.contains("x-omg-max-bytes") && text.contains(keyword),
            "{text}"
        );
    }
    // Conditional or inverted placements, at any depth, directly or through a $ref.
    for placement in [
        json!({"if":{"minLength":1},"then":limited()}),
        json!({"if":{"minLength":1},"else":limited()}),
        json!({"if":limited(),"then":{"type":"string"}}),
        json!({"not":limited()}),
        json!({"not":{"properties":{"x":limited()}}}),
    ] {
        let mut node = json!({"type":"string"});
        for (k, v) in placement.as_object().unwrap() {
            node[k] = v.clone();
        }
        let text = format!(
            "{:#}",
            check(closed(json!({"code":node.clone()})))
                .err()
                .unwrap_or_else(|| panic!("accepted {node}"))
        );
        assert!(
            text.contains("x-omg-max-bytes") && text.contains("always applies"),
            "{text}"
        );
    }
    let through_ref = json!({"components":{"schemas":{
        "Limited":limited(),
        "Probe":closed(json!({"code":{"type":"string","not":{"$ref":"#/components/schemas/Limited"}}}))}},
        "paths":{}});
    assert!(quality::completed(&through_ref).is_err());
    // The same component used plainly stays valid.
    quality::completed(&json!({"components":{"schemas":{
        "Limited":limited(),
        "Probe":closed(json!({"code":{"$ref":"#/components/schemas/Limited"}}))}},"paths":{}}))
    .unwrap();
}

// ----- round 3: arrays are tracked by schema location, not only by data location ----

#[test]
fn management_openapi_coverage_arrays_are_keyed_by_schema_pointer_too() {
    // Two `anyOf` arms declare an array at the SAME data location (`/list`): a
    // non-empty observation of one must not credit the other.
    let arm = |item: Value| {
        json!({"type":"object","required":["list"],"additionalProperties":false,
            "properties":{"list":{"type":"array","items":item}}})
    };
    let document = doc(
        op(
            json!({"anyOf":[arm(json!({"type":"string"})),arm(json!({"type":"integer"}))]}),
            json!([]),
        ),
        json!({}),
    );
    // `{"list":[]}` matches both arms; `["x"]` populates only the first arm's array.
    let gap = message(run(
        &document,
        &[json!({"list":[]}), json!({"list":["x"]})],
        &[],
    ));
    assert!(
        gap.contains("array `/list` (/response/200/anyOf/1/properties/list)")
            && !gap.contains("anyOf/0"),
        "{gap}"
    );
    // Populating the second arm's array as well closes the gap.
    run(
        &document,
        &[
            json!({"list":[]}),
            json!({"list":["x"]}),
            json!({"list":[1]}),
        ],
        &[],
    )
    .unwrap();
    // `items`, `prefixItems` and typed map values are separate schema locations
    // even though they share a wildcard data location.
    let document = doc(
        op(
            json!({"type":"array","prefixItems":[{"type":"array","items":{"type":"string"}}],
                "items":{"type":"array","items":{"type":"string"}}}),
            json!([]),
        ),
        json!({}),
    );
    let gap = message(run(&document, &[json!([["a"], []])], &[]));
    assert!(gap.contains("/response/200/items"), "{gap}");
    run(&document, &[json!([["a"], ["b"]])], &[]).unwrap();
}
