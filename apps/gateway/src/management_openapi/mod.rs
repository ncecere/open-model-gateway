//! Test-only management contract: no runtime endpoint or production dependency.
mod generator;
mod hardening_tests;
mod middleware;
mod mutation_tests;
mod quality;
pub(crate) mod requests;
pub(crate) mod responses;
mod validation;

#[cfg(test)]
mod tests {
    use super::{generator as g, validation as v};
    use serde_json::{Value, json};

    fn generated() -> Value {
        g::generate(&g::load_fragments().unwrap()).unwrap()
    }

    #[test]
    fn management_openapi_artifact_matches_regeneration() {
        let document = generated();
        g::verify(&document).unwrap();
        v::validate_document(&document).unwrap();
        let bytes = g::encode(&document);
        if std::env::var_os("OMG_REGENERATE_OPENAPI").is_some() {
            assert_eq!(std::env::var("OMG_REGENERATE_OPENAPI").unwrap(), "1");
            assert!(
                std::env::var_os("CI").is_none(),
                "regeneration forbidden in CI"
            );
            std::fs::write(g::artifact_path(), &bytes).unwrap();
        }
        assert_eq!(
            std::fs::read(g::artifact_path()).unwrap(),
            bytes,
            "management OpenAPI drift: update reviewed fragments, then explicitly regenerate"
        );
    }

    #[test]
    fn management_openapi_reports_incomplete_schemas() {
        let document = generated();
        let operations = g::operation_count(&document);
        let placeholders = g::placeholder_count(&document);
        eprintln!(
            "management OpenAPI: {operations} operations; {placeholders} incomplete request/response schemas remaining (#81)"
        );
        // Intentionally not acceptance of #81: later slices must reduce this count to zero.
        assert_eq!(
            operations, 251,
            "review route additions and update this progress baseline"
        );
        assert_eq!(
            placeholders, 350,
            "review schema completion and update this progress baseline"
        );
    }

    #[test]
    fn management_openapi_rejects_operation_and_authority_drift() {
        let good = generated();
        let path = "/api/v1/workspaces/{ws}";
        for mutation in [
            "omitted",
            "extra",
            "method",
            "template",
            "access",
            "scope",
            "token",
            "security",
            "parameter",
        ] {
            let mut bad = good.clone();
            match mutation {
                "omitted" => {
                    bad["paths"][path].as_object_mut().unwrap().remove("get");
                }
                "extra" => {
                    bad["paths"]["/api/v1/not-declared"] =
                        json!({"get":good["paths"][path]["get"]});
                }
                "method" => {
                    let op = bad["paths"][path]
                        .as_object_mut()
                        .unwrap()
                        .remove("get")
                        .unwrap();
                    bad["paths"][path]["post"] = op;
                }
                "template" => {
                    let op = bad["paths"].as_object_mut().unwrap().remove(path).unwrap();
                    bad["paths"]["/api/v1/workspaces/{workspace}"] = op;
                }
                "access" => {
                    bad["paths"][path]["get"]["x-omg-access"] = json!("Anonymous");
                }
                "scope" => {
                    bad["paths"][path]["get"]["x-omg-required-scope"] = json!("settings:write");
                }
                "token" => {
                    bad["paths"][path]["get"]["x-omg-token-policy"] = json!("Denied");
                }
                "security" => {
                    bad["paths"][path]["get"]["security"] = json!([]);
                }
                "parameter" => {
                    bad["paths"][path]["get"]["parameters"] = json!([]);
                }
                _ => unreachable!(),
            }
            assert!(g::verify(&bad).is_err(), "accepted {mutation} drift");
        }
        g::verify(&good).unwrap();
    }

    #[test]
    fn management_openapi_requires_matching_reviewed_fragments() {
        let good = g::load_fragments().unwrap();
        let key = "GET /api/v1/workspaces/{ws}";
        for mutation in ["missing", "extra", "access", "scope", "override"] {
            let mut bad = good.clone();
            match mutation {
                "missing" => {
                    bad.operations.remove(key);
                }
                "extra" => {
                    bad.operations
                        .insert("GET /api/v1/unknown".into(), bad.operations[key].clone());
                }
                "access" => {
                    bad.operations.get_mut(key).unwrap().access = "Anonymous".into();
                }
                "scope" => {
                    bad.operations.get_mut(key).unwrap().scope = Some("settings:write".into());
                }
                "override" => {
                    bad.operations
                        .get_mut(key)
                        .unwrap()
                        .operation
                        .insert("security".into(), json!([]));
                }
                _ => unreachable!(),
            }
            assert!(g::generate(&bad).is_err(), "accepted {mutation} fragment");
        }
        let dir = tempfile::tempdir().unwrap();
        let bytes = serde_json::to_vec(&good).unwrap();
        std::fs::write(dir.path().join("a.json"), &bytes).unwrap();
        std::fs::write(dir.path().join("b.json"), &bytes).unwrap();
        assert!(
            g::load_fragments_from(dir.path()).is_err(),
            "duplicate operation fragments"
        );
        let dir = tempfile::tempdir().unwrap();
        let entry = serde_json::to_string(&good.operations[key]).unwrap();
        let duplicate = format!("{{\"operations\":{{\"{key}\":{entry},\"{key}\":{entry}}}}}");
        std::fs::write(dir.path().join("duplicate.json"), duplicate).unwrap();
        assert!(
            g::load_fragments_from(dir.path()).is_err(),
            "duplicate key in one fragment file"
        );
    }

    #[test]
    fn management_openapi_rejects_unresolved_and_external_references() {
        let good = generated();
        for reference in [
            "#/components/schemas/Missing",
            "https://example.invalid/schema",
            "file:///etc/passwd",
        ] {
            assert!(
                v::compile_schema(&json!({"$ref":reference})).is_err(),
                "retriever accepted unresolved or external reference {reference}"
            );
            let mut bad = good.clone();
            bad["components"]["schemas"]["Probe"] = json!({"$ref":reference});
            assert!(
                v::validate_document(&bad).is_err(),
                "accepted ref {reference}"
            );
        }
        v::validate_document(&good).unwrap();
    }

    #[test]
    fn management_openapi_rejects_response_links() {
        let good = generated();
        let (path, item) = good["paths"].as_object().unwrap().iter().next().unwrap();
        let (method, operation) = item.as_object().unwrap().iter().next().unwrap();
        let status = operation["responses"]
            .as_object()
            .unwrap()
            .keys()
            .next()
            .unwrap()
            .clone();
        let mut bad = good.clone();
        bad["paths"][path][method]["responses"][&status]["links"] =
            json!({"Next":{"operationId":"doesNotExist"}});
        assert!(
            v::validate_document(&bad).is_err(),
            "accepted a dangling link"
        );
        let mut components = good.clone();
        components["components"]["links"] = json!({"Next":{"operationId":"doesNotExist"}});
        assert!(v::validate_document(&components).is_err());
        v::validate_document(&good).unwrap();
    }

    #[test]
    fn management_openapi_rejects_undocumented_reasons() {
        let mut bad = generated();
        bad["components"]["schemas"]["ErrorEnvelope"]["properties"]["error"]["properties"]["reason"]["enum"]
            .as_array_mut().unwrap().pop();
        assert!(g::verify(&bad).is_err());
    }

    #[tokio::test]
    async fn management_openapi_shared_error_serialization() {
        use crate::management::{ApiError, POLICY_REASONS, REASONS};
        use axum::{http::StatusCode, response::IntoResponse};

        let document = generated();
        let validator =
            v::compile_schema(&document["components"]["schemas"]["ErrorEnvelope"]).unwrap();
        for (message, reason) in REASONS
            .iter()
            .map(|(m, r)| (*m, *r))
            .chain(POLICY_REASONS.iter().map(|(m, r, _, _)| (*m, *r)))
        {
            let response = ApiError(StatusCode::BAD_REQUEST, message).into_response();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
            let bytes = axum::body::to_bytes(response.into_body(), 16_384)
                .await
                .unwrap();
            let body: Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(body["error"]["code"], "400");
            assert_eq!(body["error"]["reason"], reason);
            validator.validate(&body).unwrap();
        }
        let response = ApiError(StatusCode::NOT_FOUND, "Not found").into_response();
        let bytes = axum::body::to_bytes(response.into_body(), 16_384)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert!(body["error"].get("reason").is_none());
        validator.validate(&body).unwrap();
    }

    #[test]
    fn management_openapi_metaschema_and_schema_controls() {
        let good = generated();
        v::validate_document(&good).unwrap();
        for mutation in ["openapi", "responses", "schema"] {
            let mut bad = good.clone();
            match mutation {
                "openapi" => bad["openapi"] = json!("3.0.3"),
                "responses" => bad["paths"]["/api/v1/me"]["get"]["responses"] = json!(false),
                "schema" => {
                    bad["components"]["schemas"]["ErrorEnvelope"]["type"] = json!("not-a-type")
                }
                _ => unreachable!(),
            }
            assert!(
                v::validate_document(&bad).is_err(),
                "accepted {mutation} corruption"
            );
        }
    }

    #[test]
    fn management_openapi_regex_errors_fail_closed() {
        // The production gate uses a linear-time backend: unsupported look-around,
        // invalid regexes and excessive compilation complexity are errors, never non-matches.
        for schema in [
            json!({"pattern":"["}),
            json!({"patternProperties":{"[":false}}),
            json!({"not":{"pattern":"(?=a)a"}}),
            json!({"patternProperties":{"(?=a)a":false}}),
        ] {
            assert!(
                v::compile_schema(&schema).is_err(),
                "accepted unexecutable regex {schema}"
            );
            let mut fragments = g::load_fragments().unwrap();
            fragments.schemas.insert("InvalidRegex".into(), schema);
            assert!(
                g::generate(&fragments)
                    .and_then(|doc| v::validate_document(&doc))
                    .is_err()
            );
        }
        let validator = v::compile_schema(&json!({"patternProperties":{"^a":false}})).unwrap();
        assert!(validator.validate(&json!({"b":1})).is_ok());
        assert!(validator.validate(&json!({"a":1})).is_err());
    }

    #[test]
    fn management_openapi_security_alternatives_are_exact() {
        let doc = generated();
        assert_eq!(
            doc["paths"]["/api/v1/auth/config"]["get"]["security"],
            json!([])
        );
        assert_eq!(
            doc["paths"]["/api/v1/auth/logout"]["post"]["security"],
            json!([{"browserSession":[],"csrfToken":[],"origin":[]} ])
        );
        let admin = &doc["paths"]["/api/v1/platform/admin-tokens"];
        assert_eq!(admin["get"]["x-omg-token-policy"], "BrowserAdminOnly");
        assert_eq!(admin["get"]["security"], json!([{"browserSession":[]} ]));
        let scoped = &doc["paths"]["/api/v1/workspaces/{ws}"]["get"];
        assert_eq!(
            scoped["security"],
            json!([{"browserSession":[]},{"adminToken":[]} ])
        );
        assert_eq!(scoped["x-omg-required-scope"], "platform:read");
    }

    #[test]
    fn management_openapi_vendor_checksums_are_enforced() {
        let mut files = v::vendor_files().unwrap();
        v::verify_vendor(&files).unwrap();
        files.values_mut().next().unwrap().push(b' ');
        assert!(v::verify_vendor(&files).is_err());
    }
}
