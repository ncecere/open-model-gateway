//! Route parity, authority and fragment drift gates of the inference artifact.
//! DB-free: the real router is built over a lazy pool that is never connected.
use super::{generator as g, tests::generated};
use crate::management::openapi::generator as shared;
use serde_json::{Value, json};
use std::collections::BTreeSet;

/// The (METHOD, template) pairs of a document.
fn operations(document: &Value) -> BTreeSet<(String, String)> {
    document["paths"]
        .as_object()
        .unwrap()
        .iter()
        .flat_map(|(path, item)| {
            item.as_object()
                .unwrap()
                .keys()
                .map(|method| (method.to_ascii_uppercase(), path.clone()))
        })
        .collect()
}

#[test]
fn inference_openapi_route_parity_is_three_way() {
    // The compiled policy table, the source scan of every route registration
    // (independent of the table) and the artifact must name the same operations.
    let table: BTreeSet<(String, String)> = crate::route_policy::ROUTES
        .iter()
        .filter(|r| r.path.starts_with("/v1/"))
        .flat_map(|r| {
            r.methods
                .split_whitespace()
                .map(move |m| (m.to_owned(), r.path.to_owned()))
        })
        .collect();
    let scan: BTreeSet<(String, String)> = crate::authz_matrix::inventory()
        .into_iter()
        .filter(|(_, path)| path.starts_with("/v1/"))
        .collect();
    let artifact = operations(&generated());
    assert_eq!(table.len(), 27, "the inference route inventory changed");
    assert_eq!(table, scan, "route table and source scan disagree");
    assert_eq!(table, artifact, "route table and artifact disagree");
}

#[test]
fn inference_openapi_rejects_operation_and_authority_drift() {
    let good = generated();
    let path = "/v1/files/{id}";
    let messages = "/v1/messages";
    for mutation in [
        "omitted",
        "extra",
        "method",
        "template",
        "access",
        "scope",
        "token",
        "security",
        "security-alternative-removed",
        "security-alternative-added",
        "path-parameter",
        "invented-query-parameter",
        "dropped-query-parameter",
        "management-path",
    ] {
        let mut bad = good.clone();
        match mutation {
            "omitted" => {
                bad["paths"][path].as_object_mut().unwrap().remove("get");
            }
            "extra" => {
                bad["paths"]["/v1/not-declared"] = json!({"get":good["paths"][path]["get"]});
            }
            "method" => {
                let op = bad["paths"][path]
                    .as_object_mut()
                    .unwrap()
                    .remove("get")
                    .unwrap();
                bad["paths"][path]["put"] = op;
            }
            "template" => {
                let op = bad["paths"].as_object_mut().unwrap().remove(path).unwrap();
                bad["paths"]["/v1/files/{file}"] = op;
            }
            "access" => {
                bad["paths"][path]["get"]["x-omg-access"] = json!("Browser");
            }
            "scope" => {
                bad["paths"][path]["get"]["x-omg-required-scope"] = json!("platform:read");
            }
            "token" => {
                bad["paths"][path]["get"]["x-omg-token-policy"] = json!("BrowserAdminOnly");
            }
            "security" => {
                bad["paths"][path]["get"]["security"] = json!([]);
            }
            "security-alternative-removed" => {
                bad["paths"][messages]["post"]["security"] = json!([{"inferenceKey":[]}]);
            }
            "security-alternative-added" => {
                bad["paths"]["/v1/embeddings"]["post"]["security"] =
                    json!([{"inferenceKey":[]},{"anthropicApiKey":[]}]);
            }
            "path-parameter" => {
                bad["paths"][path]["get"]["parameters"] = json!([]);
            }
            "invented-query-parameter" => {
                bad["paths"]["/v1/models"]["get"]["parameters"] =
                    json!([{"name":"workspace_id","in":"query","schema":{"type":"string"}}]);
            }
            "dropped-query-parameter" => {
                bad["paths"]["/v1/realtime"]["get"]["parameters"] = json!([]);
            }
            "management-path" => {
                bad["paths"]["/api/v1/me"] = json!({"get":good["paths"][path]["get"]});
            }
            _ => unreachable!(),
        }
        assert!(g::verify(&bad).is_err(), "accepted {mutation} drift");
    }
    g::verify(&good).unwrap();
}

#[test]
fn inference_openapi_requires_matching_reviewed_fragments() {
    let good = g::load_fragments().unwrap();
    let key = "POST /v1/embeddings";
    for mutation in ["missing", "extra", "access", "scope", "token", "override"] {
        let mut bad = good.clone();
        match mutation {
            "missing" => {
                bad.operations.remove(key);
            }
            "extra" => {
                bad.operations
                    .insert("GET /v1/unknown".into(), bad.operations[key].clone());
            }
            "access" => {
                bad.operations.get_mut(key).unwrap().access = "Browser".into();
            }
            "scope" => {
                bad.operations.get_mut(key).unwrap().scope = Some("platform:read".into());
            }
            "token" => {
                bad.operations.get_mut(key).unwrap().token_policy = "BrowserAdminOnly".into();
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
    // The generated vocabulary and envelopes cannot be overridden by a fragment.
    for name in [
        "OpenAiErrorEnvelope",
        "OpenAiErrorCode",
        "AnthropicErrorEnvelope",
    ] {
        let mut bad = good.clone();
        bad.schemas.insert(name.into(), json!({"type":"object"}));
        assert!(g::generate(&bad).is_err(), "a fragment overrode {name}");
    }
    let dir = tempfile::tempdir().unwrap();
    let bytes = serde_json::to_vec(&good).unwrap();
    std::fs::write(dir.path().join("a.json"), &bytes).unwrap();
    std::fs::write(dir.path().join("b.json"), &bytes).unwrap();
    assert!(
        shared::load_fragments_from(dir.path()).is_err(),
        "duplicate operation fragments"
    );
    let dir = tempfile::tempdir().unwrap();
    let entry = serde_json::to_string(&good.operations[key]).unwrap();
    let duplicate = format!("{{\"operations\":{{\"{key}\":{entry},\"{key}\":{entry}}}}}");
    std::fs::write(dir.path().join("duplicate.json"), duplicate).unwrap();
    assert!(
        shared::load_fragments_from(dir.path()).is_err(),
        "duplicate key in one fragment file"
    );
}

#[test]
fn inference_openapi_security_alternatives_are_exact() {
    let doc = generated();
    assert_eq!(
        doc["paths"]["/v1/models"]["get"]["security"],
        json!([{"inferenceKey":[]}])
    );
    assert_eq!(
        doc["paths"]["/v1/messages"]["post"]["security"],
        json!([{"inferenceKey":[]},{"anthropicApiKey":[]}])
    );
    assert_eq!(
        doc["paths"]["/v1/realtime"]["get"]["security"],
        json!([{"inferenceKey":[]},{"realtimeSubprotocol":[]}])
    );
    for (path, item) in doc["paths"].as_object().unwrap() {
        for (method, op) in item.as_object().unwrap() {
            assert_eq!(op["x-omg-access"], "InferenceKey", "{method} {path}");
            assert_eq!(op["x-omg-token-policy"], "Denied", "{method} {path}");
            assert!(op.get("x-omg-required-scope").is_none());
        }
    }
    let schemes = doc["components"]["securitySchemes"].as_object().unwrap();
    assert_eq!(
        schemes.keys().map(String::as_str).collect::<Vec<_>>(),
        ["anthropicApiKey", "inferenceKey", "realtimeSubprotocol"]
    );
    assert!(
        schemes
            .keys()
            .all(|name| !name.contains("admin") && !name.contains("browser"))
    );
}

/// The security alternatives are proven against the REAL credential parsers:
/// a well-formed credential in an accepted form reaches key authentication
/// (503 here, the lazy database is unreachable); anything else is a 401 before
/// any lookup.
#[tokio::test]
async fn inference_openapi_security_matches_the_real_credential_parsers() {
    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
    use tower::ServiceExt;
    let doc = generated();
    let app = || {
        // sqlx retries a refused connection until the acquire timeout: keep it short.
        let pool = sqlx::postgres::PgPoolOptions::new()
            .acquire_timeout(std::time::Duration::from_millis(50))
            .connect_lazy("postgres://unused:unused@127.0.0.1:1/unused")
            .unwrap();
        crate::http::router(crate::store::Store::new(pool))
    };
    let token = format!("omg_{}.{}", "0".repeat(32), "0".repeat(64));
    let token = token.as_str();
    for (path, item) in doc["paths"].as_object().unwrap() {
        for (method, op) in item.as_object().unwrap() {
            let accepts = |scheme: &str| {
                op["security"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|alternative| alternative.get(scheme).is_some())
            };
            let url = path.replace("{id}", "id")
                + if path == "/v1/realtime" {
                    "?model=m"
                } else {
                    ""
                };
            let send = |headers: &[(&str, String)]| {
                let mut request = Request::builder()
                    .method(method.to_ascii_uppercase().as_str())
                    .uri(&url);
                for (name, value) in headers {
                    request = request.header(*name, value);
                }
                let request = request.body(Body::empty()).unwrap();
                let app = app();
                async move { app.oneshot(request).await.unwrap().status() }
            };
            let bearer = ("authorization", format!("Bearer {token}"));
            let api_key = ("x-api-key", token.to_owned());
            let subprotocol = (
                "sec-websocket-protocol",
                format!("realtime, openai-insecure-api-key.{token}"),
            );
            let reached = StatusCode::SERVICE_UNAVAILABLE;
            let refused = StatusCode::UNAUTHORIZED;
            let case = |accepted: bool| if accepted { reached } else { refused };
            assert_eq!(
                send(std::slice::from_ref(&bearer)).await,
                reached,
                "{method} {path} bearer"
            );
            assert_eq!(
                send(std::slice::from_ref(&api_key)).await,
                case(accepts("anthropicApiKey")),
                "{method} {path} x-api-key"
            );
            assert_eq!(
                send(std::slice::from_ref(&subprotocol)).await,
                case(accepts("realtimeSubprotocol")),
                "{method} {path} subprotocol"
            );
            // Never two credentials, never none.
            assert_eq!(
                send(&[bearer.clone(), api_key]).await,
                refused,
                "{method} {path} both"
            );
            assert_eq!(send(&[]).await, refused, "{method} {path} none");
            if accepts("realtimeSubprotocol") {
                assert_eq!(
                    send(&[bearer, subprotocol]).await,
                    refused,
                    "{method} {path} both"
                );
            }
        }
    }
}

#[test]
fn inference_openapi_completed_areas_have_no_incomplete_references() {
    use super::responses::{AREA_GROUPS, area, completed, require_complete};
    let doc = generated();
    let mut count = 0;
    for (path, item) in doc["paths"].as_object().unwrap() {
        let is_completed = completed(area(path));
        for (method, op) in item.as_object().unwrap() {
            if is_completed {
                require_complete(&doc, op).unwrap_or_else(|e| panic!("{method} {path}: {e:#}"));
                assert!(
                    op["responses"].get("default").is_none(),
                    "a completed operation needs exact statuses"
                );
                count += 1;
            } else {
                // An incomplete operation must say so rather than look finished.
                assert!(
                    op["x-omg-request-schema"].is_object() && require_complete(&doc, op).is_err(),
                    "{method} {path} is neither completed nor an honest placeholder"
                );
            }
        }
    }
    assert_eq!(count, 7);
    // Groups are disjoint and every area names real operations.
    let mut seen = BTreeSet::new();
    for (_, group) in AREA_GROUPS {
        for area_name in *group {
            assert!(seen.insert(*area_name), "area {area_name} owned twice");
            assert!(
                doc["paths"]
                    .as_object()
                    .unwrap()
                    .keys()
                    .any(|path| area(path) == *area_name),
                "area {area_name} has no operations"
            );
        }
    }
}

#[test]
fn inference_openapi_areas_are_fine_grained() {
    use super::responses::area;
    for (template, want) in [
        ("/v1/models", "models"),
        ("/v1/chat/completions", "chat"),
        ("/v1/files/{id}/content", "files"),
        ("/v1/realtime", "realtime"),
        ("/v1/realtime/calls", "realtime/calls"),
        ("/v1/realtime/client_secrets", "realtime/client_secrets"),
        ("/api/v1/me", ""),
    ] {
        assert_eq!(area(template), want, "{template}");
    }
}

/// A registered group is finished by an executed `#[sqlx::test]` fixture that
/// calls `finish(<GROUP>)` directly: helper-only calls and comments do not count.
#[test]
fn inference_openapi_area_groups_are_bound_to_executed_fixtures() {
    use syn::visit::Visit;
    struct Finish {
        in_sqlx_test: bool,
        finished: BTreeSet<String>,
    }
    impl<'ast> Visit<'ast> for Finish {
        fn visit_item_fn(&mut self, function: &'ast syn::ItemFn) {
            let outer = self.in_sqlx_test;
            self.in_sqlx_test = function.attrs.iter().any(|attr| {
                attr.path()
                    .segments
                    .iter()
                    .map(|s| s.ident.to_string())
                    .collect::<Vec<_>>()
                    == ["sqlx", "test"]
            });
            syn::visit::visit_item_fn(self, function);
            self.in_sqlx_test = outer;
        }
        fn visit_expr_method_call(&mut self, call: &'ast syn::ExprMethodCall) {
            if self.in_sqlx_test
                && call.method == "finish"
                && let Some(syn::Expr::Path(path)) = call.args.first()
                && let Some(last) = path.path.segments.last()
            {
                self.finished.insert(last.ident.to_string());
            }
            syn::visit::visit_expr_method_call(self, call);
        }
    }
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src/inference_openapi/fixtures_tests.rs");
    let source = syn::parse_file(&std::fs::read_to_string(path).unwrap()).unwrap();
    let mut visitor = Finish {
        in_sqlx_test: false,
        finished: BTreeSet::new(),
    };
    visitor.visit_file(&source);
    for (name, _) in super::responses::AREA_GROUPS {
        assert!(
            visitor.finished.contains(*name),
            "{name} is registered but no #[sqlx::test] calls finish({name})"
        );
    }
}
