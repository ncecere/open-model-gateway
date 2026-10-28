//! Per-operation negative controls, using uncached document validation. Real
//! handler positives remain in the PostgreSQL fixture registry, not this file.
use super::responses::*;
use axum::http::{HeaderMap, StatusCode};
use serde_json::{Value, json};

fn sample(document: &Value, schema: &Value) -> Value {
    if let Some(r) = schema["$ref"].as_str() {
        return sample(document, document.pointer(&r[1..]).unwrap());
    }
    if let Some(v) = schema.get("const") {
        return v.clone();
    }
    if let Some(v) = schema["enum"].as_array() {
        return v[0].clone();
    }
    let ty = schema["type"]
        .as_str()
        .or_else(|| schema["type"].as_array().and_then(|a| a[0].as_str()));
    let mut value = match ty {
        Some("object") => json!({}),
        Some("array") => {
            if let Some(prefix) = schema["prefixItems"].as_array() {
                json!(
                    prefix
                        .iter()
                        .map(|s| sample(document, s))
                        .collect::<Vec<_>>()
                )
            } else {
                json!([sample(document, &schema["items"])])
            }
        }
        Some("integer" | "number") => json!(schema["minimum"].as_i64().unwrap_or(1)),
        Some("boolean") => json!(false),
        Some("null") => Value::Null,
        Some("string") => json!(match schema["format"].as_str() {
            Some("uuid") => uuid::Uuid::nil().to_string(),
            Some("date-time") => "2026-10-28T00:00:00Z".into(),
            Some("date") => "2026-10-28".into(),
            _ if schema["pattern"]
                .as_str()
                .is_some_and(|p| p.contains("branding/logo")) =>
                "/api/v1/branding/logo?v=000000000000".into(),
            _ if schema.get("pattern").is_some() => "0".into(),
            _ => "fixture".into(),
        }),
        _ => json!({}), // conjunctive refinement or the reviewed stored JSON field
    };
    let required = schema["required"].as_array();
    if let Some(properties) = schema["properties"].as_object() {
        for (name, schema) in properties {
            // A `not`/if-only guard narrows the existing value; it supplies no
            // new instance value. The final validator independently checks it.
            if ["type", "$ref", "const", "enum", "allOf", "anyOf", "oneOf"]
                .iter()
                .any(|k| schema.get(k).is_some())
                || required.is_some_and(|r| r.iter().any(|v| v.as_str() == Some(name.as_str())))
            {
                value[name] = sample(document, schema);
            }
        }
    }
    if let Some(all) = schema["allOf"].as_array() {
        for schema in all {
            let next = sample(document, schema);
            if let (Some(out), Some(input)) = (value.as_object_mut(), next.as_object()) {
                out.extend(input.clone());
            } else {
                value = next;
            }
        }
    }
    for key in ["oneOf", "anyOf"] {
        if let Some(branches) = schema[key].as_array() {
            // Populate nullable object/array representations too, rather than
            // letting a leading null hide their nested mutation controls.
            let branch = branches
                .iter()
                .find(|b| b["type"] != "null")
                .unwrap_or(&branches[0]);
            let next = sample(document, branch);
            if let (Some(out), Some(input)) = (value.as_object_mut(), next.as_object()) {
                out.extend(input.clone());
            } else {
                value = next;
            }
        }
    }
    value
}

fn recursive_mutations(document: &Value, schema: &Value, body: &Value) -> Vec<(String, Value)> {
    use std::collections::BTreeSet;
    fn walk(
        document: &Value,
        schema: &Value,
        value: &Value,
        path: &str,
        targets: &mut BTreeSet<String>,
        seen: &mut BTreeSet<(String, String)>,
    ) {
        // Stored arbitrary JSON intentionally accepts arbitrary nested values.
        // Match the reviewed schema node itself, never a copied comment.
        if super::quality::ARBITRARY_JSON
            .iter()
            .any(|(pointer, reason)| {
                document
                    .pointer(pointer)
                    .is_some_and(|node| std::ptr::eq(node, schema))
                    && schema["$comment"] == *reason
            })
        {
            return;
        }
        if let Some(reference) = schema["$ref"].as_str()
            && seen.insert((reference.into(), path.into()))
        {
            walk(
                document,
                document.pointer(&reference[1..]).unwrap(),
                value,
                path,
                targets,
                seen,
            );
        }
        if value.is_object() && schema["additionalProperties"] == false || value.is_array() {
            targets.insert(path.into());
        }
        for keyword in ["allOf", "anyOf", "oneOf"] {
            for branch in schema[keyword].as_array().into_iter().flatten() {
                if keyword == "allOf" || validator(document, branch).unwrap().is_valid(value) {
                    walk(document, branch, value, path, targets, seen);
                }
            }
        }
        if let Some(condition) = schema.get("if") {
            let branch = if validator(document, condition).unwrap().is_valid(value) {
                "then"
            } else {
                "else"
            };
            if let Some(branch) = schema.get(branch) {
                walk(document, branch, value, path, targets, seen);
            }
        }
        for (name, child) in schema["properties"].as_object().into_iter().flatten() {
            if let Some(value) = value.get(name) {
                let name = name.replace('~', "~0").replace('/', "~1");
                walk(
                    document,
                    child,
                    value,
                    &format!("{path}/{name}"),
                    targets,
                    seen,
                );
            }
        }
        for (i, value) in value.as_array().into_iter().flatten().enumerate() {
            if let Some(child) = schema["prefixItems"].get(i).or_else(|| schema.get("items")) {
                walk(
                    document,
                    child,
                    value,
                    &format!("{path}/{i}"),
                    targets,
                    seen,
                );
            }
        }
    }
    let mut targets = BTreeSet::new();
    walk(
        document,
        schema,
        body,
        "",
        &mut targets,
        &mut BTreeSet::new(),
    );
    targets
        .into_iter()
        .map(|path| {
            let mut corrupt = body.clone();
            let target = corrupt.pointer_mut(&path).unwrap();
            if let Some(array) = target.as_array_mut() {
                array.push(json!({"omg_session":"LEAK"}));
            } else {
                target["omg_session"] = json!("LEAK");
            }
            (path, corrupt)
        })
        .collect()
}

#[test]
fn management_openapi_hardening_response_mutations_reach_nested_objects_and_arrays() {
    let schema = json!({"type":"object","required":["data"],"additionalProperties":false,"properties":{"data":{"type":"array","items":{"type":"object","required":["names"],"additionalProperties":false,"properties":{"names":{"type":"array","items":{"type":"string"}}}}}}});
    let document = json!({"components":{"schemas":{"Response":schema}}});
    for schema in [
        schema,
        json!({"allOf":[{"$ref":"#/components/schemas/Response"}]}),
    ] {
        for body in [
            json!({"data":[{"names":["safe"]}]}),
            json!({"data":[{"names":[]}]}),
        ] {
            validate_value(&document, &schema, &body).unwrap();
            let mutations = recursive_mutations(&document, &schema, &body);
            assert_eq!(
                mutations
                    .iter()
                    .map(|(path, _)| path.as_str())
                    .collect::<std::collections::BTreeSet<_>>(),
                std::collections::BTreeSet::from(["", "/data", "/data/0", "/data/0/names"])
            );
            for (path, mutation) in mutations {
                assert!(
                    validate_value(&document, &schema, &mutation).is_err(),
                    "accepted mutation at {path}"
                );
            }
        }
    }
}

#[test]
fn management_openapi_review_every_completed_operation_rejects_response_mutations() {
    let document = artifact().clone(); // deliberately NOT the immutable cached pointer
    let mut operations = 0;
    let mut json_operations = 0;
    for (path, item) in document["paths"].as_object().unwrap() {
        if !completed(area(path)) {
            continue;
        }
        for (method, op) in item.as_object().unwrap() {
            let method = method.to_ascii_uppercase();
            operations += 1;
            let mut h = HeaderMap::new();
            if let Some(schema) =
                op["responses"]["200"]["content"]["application/json"].get("schema")
            {
                json_operations += 1;
                h.insert("content-type", "application/json".parse().unwrap());
                h.insert("cache-control", "no-store".parse().unwrap());
                h.insert("referrer-policy", "no-referrer".parse().unwrap());
                h.insert("x-content-type-options", "nosniff".parse().unwrap());
                if path.ends_with("/export") {
                    h.insert("cache-control", "no-store".parse().unwrap());
                    h.insert(
                        "content-disposition",
                        "attachment; filename=\"omg-export-2026-10-28-2026-10-28.json\""
                            .parse()
                            .unwrap(),
                    );
                    h.insert("x-omg-export-sha256", "a".repeat(64).parse().unwrap());
                }
                let valid = sample(&document, schema);
                let encode = |v: &Value| serde_json::to_vec(v).unwrap();
                validate(
                    &document,
                    &method,
                    path,
                    StatusCode::OK,
                    &h,
                    &encode(&valid),
                )
                .unwrap_or_else(|e| {
                    panic!("positive schema control {method} {path}: {e:#}; {valid}")
                });
                let mut body_schema = schema;
                while let Some(reference) = body_schema["$ref"].as_str() {
                    body_schema = document.pointer(&reference[1..]).unwrap();
                }
                let field = body_schema["required"].as_array().unwrap()[0]
                    .as_str()
                    .unwrap();
                let mut absent = valid.clone();
                absent.as_object_mut().unwrap().remove(field);
                let mut wrong = valid.clone();
                wrong[field] = if valid[field].is_boolean() {
                    json!("not-a-boolean")
                } else {
                    json!(false)
                };
                for (pointer, corrupt) in recursive_mutations(&document, schema, &valid) {
                    assert!(
                        validate(
                            &document,
                            &method,
                            path,
                            StatusCode::OK,
                            &h,
                            &encode(&corrupt)
                        )
                        .is_err(),
                        "accepted recursive sensitive property/array element at {pointer}: {method} {path}"
                    );
                }
                let mut extra = valid.clone();
                extra["plaintext_inference_token"] = json!("must-not-leak");
                for (label, corrupt) in [
                    ("missing required field", absent),
                    ("wrong required-field type", wrong),
                    ("sensitive unexpected property", extra),
                ] {
                    assert!(
                        validate(
                            &document,
                            &method,
                            path,
                            StatusCode::OK,
                            &h,
                            &encode(&corrupt)
                        )
                        .is_err(),
                        "accepted {label}: {method} {path}"
                    );
                }
            } else if op["responses"]["200"]["content"].get("text/csv").is_some() {
                let scheduled = path.ends_with("/csv");
                h.insert("content-type", "text/csv; charset=utf-8".parse().unwrap());
                h.insert("cache-control", "no-store".parse().unwrap());
                h.insert("x-content-type-options", "nosniff".parse().unwrap());
                h.insert("x-export-limit", "1000".parse().unwrap());
                h.insert("x-export-rows", "1".parse().unwrap());
                h.insert("x-export-offset", "0".parse().unwrap());
                h.insert(
                    "content-disposition",
                    if scheduled {
                        "attachment; filename=\"scheduled-report.csv\""
                    } else {
                        "attachment; filename=\"usage.csv\""
                    }
                    .parse()
                    .unwrap(),
                );
                let bytes = if scheduled {
                    b"dimension,id,name,known_cost_microusd,\r\n".as_slice()
                } else {
                    b"id,root_request_id,attempt_number,\r\n".as_slice()
                };
                validate(&document, &method, path, StatusCode::OK, &h, bytes).unwrap();
                let mut missing = h.clone();
                missing.remove("content-disposition");
                assert!(
                    validate(&document, &method, path, StatusCode::OK, &missing, bytes).is_err()
                );
                let mut wrong = h.clone();
                wrong.insert("content-disposition", "false".parse().unwrap());
                assert!(validate(&document, &method, path, StatusCode::OK, &wrong, bytes).is_err());
                assert!(
                    validate(
                        &document,
                        &method,
                        path,
                        StatusCode::OK,
                        &h,
                        b"{\"plaintext_inference_token\":\"must-not-leak\"}"
                    )
                    .is_err()
                );
            } else {
                // These operations have no JSON representation: remove a
                // required wire header, corrupt its type/format, and inject
                // an unexpected sensitive JSON body. Each must fail too.
                let status = if path == "/api/v1/branding/logo" {
                    StatusCode::OK
                } else if method == "POST" {
                    StatusCode::NO_CONTENT
                } else {
                    StatusCode::SEE_OTHER
                };
                h.insert(
                    "cache-control",
                    if status == StatusCode::OK {
                        "public, max-age=300"
                    } else {
                        "no-store"
                    }
                    .parse()
                    .unwrap(),
                );
                h.insert("referrer-policy", "no-referrer".parse().unwrap());
                let image = crate::management::branding::fixtures::png(16, 16);
                let (header, valid_body) = if status == StatusCode::OK {
                    h.insert("content-type", "image/png".parse().unwrap());
                    h.insert("etag", format!("\"{}\"", "a".repeat(64)).parse().unwrap());
                    h.insert("x-content-type-options", "nosniff".parse().unwrap());
                    h.insert(
                        "content-security-policy",
                        "default-src 'none'".parse().unwrap(),
                    );
                    h.insert(
                        "cross-origin-resource-policy",
                        "same-origin".parse().unwrap(),
                    );
                    ("etag", image.as_slice())
                } else {
                    h.insert(
                        "location",
                        if path.ends_with("login") {
                            "https://issuer.invalid/authorize"
                        } else {
                            "/"
                        }
                        .parse()
                        .unwrap(),
                    );
                    for (name, value, site, http_only, age) in [
                        (
                            "omg_session",
                            if status == StatusCode::NO_CONTENT {
                                String::new()
                            } else {
                                "a".repeat(64)
                            },
                            "Lax",
                            "; HttpOnly",
                            if status == StatusCode::NO_CONTENT {
                                "0"
                            } else {
                                "43200"
                            },
                        ),
                        (
                            "omg_csrf",
                            if status == StatusCode::NO_CONTENT {
                                String::new()
                            } else {
                                "a".repeat(64)
                            },
                            "Strict",
                            "",
                            if status == StatusCode::NO_CONTENT {
                                "0"
                            } else {
                                "43200"
                            },
                        ),
                        (
                            "omg_oidc",
                            if path.ends_with("login") {
                                "a".repeat(64)
                            } else {
                                String::new()
                            },
                            "Lax",
                            "; HttpOnly",
                            if path.ends_with("login") { "600" } else { "0" },
                        ),
                    ] {
                        if path.ends_with("login") && name != "omg_oidc" {
                            continue;
                        }
                        h.append(
                            "set-cookie",
                            format!(
                                "{name}={value}; Path=/; Max-Age={age}; SameSite={site}{http_only}"
                            )
                            .parse()
                            .unwrap(),
                        );
                    }
                    (
                        if status == StatusCode::NO_CONTENT {
                            "set-cookie"
                        } else {
                            "location"
                        },
                        b"".as_slice(),
                    )
                };
                validate(&document, &method, path, status, &h, valid_body).unwrap();
                let mut missing = h.clone();
                missing.remove(header);
                assert!(
                    validate(&document, &method, path, status, &missing, valid_body).is_err(),
                    "missing required header {method} {path}"
                );
                let mut wrong = h.clone();
                wrong.remove(header);
                wrong.insert(header, "false".parse().unwrap());
                assert!(
                    validate(&document, &method, path, status, &wrong, valid_body).is_err(),
                    "wrong header type/format {method} {path}"
                );
                let extra = h; // keep the declared media type; do not hide a body mutation behind a MIME mutation
                assert!(
                    validate(
                        &document,
                        &method,
                        path,
                        status,
                        &extra,
                        b"{\"plaintext_inference_token\":\"must-not-leak\"}"
                    )
                    .is_err(),
                    "sensitive unexpected representation {method} {path}"
                );
            }
        }
    }
    assert_eq!((operations, json_operations), (69, 62));
}

#[test]
fn management_openapi_review_only_immutable_artifact_validators_are_cached() {
    let schema = json!({"$ref":"#/components/schemas/get_api_v1_auth_configResponse"});
    let a = super::responses::validator(artifact(), &schema).unwrap();
    let b = super::responses::validator(artifact(), &schema).unwrap();
    assert!(std::sync::Arc::ptr_eq(&a, &b));
    let mut changed = artifact().clone();
    changed["components"]["schemas"]["get_api_v1_auth_configResponse"] = json!({"const":"mutated"});
    let fresh = super::responses::validator(&changed, &schema).unwrap();
    assert!(!std::sync::Arc::ptr_eq(&a, &fresh));
    assert!(fresh.is_valid(&json!("mutated")));
    assert!(!a.is_valid(&json!("mutated")));
}
