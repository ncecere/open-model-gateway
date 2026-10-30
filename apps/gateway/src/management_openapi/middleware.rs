//! Shared contracts of the actual serving layers (not the isolated policy stubs).
//!
//! Inventory: http::build_router and the upload routers' body-limit layers
//! => 413 text for every management operation. identity::authenticate_management => 401 for
//! duplicate Authorization, mixed credential families, bad/missing/stale
//! cookies or bearer credentials; 403 for unsafe browser Origin/CSRF; 429 for
//! per-IP, replica and authenticated-token throttles; 500 for auth storage.
//! management::authorization => 403 token-policy/scope/IP/role refusal, 404
//! workspace/key hiding, 503 authorization/audit storage, and 501 for an
//! otherwise-admitted Scope route not in TOKEN_READY_ROUTES.
//! Logout is NOT mounted behind these management layers: it calls
//! verify_session/verify_csrf itself (401/403/500, no bearer throttle).
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};

fn reference(document: &Value, value: &Value) -> Result<Value> {
    let mut value = value;
    let mut seen = std::collections::BTreeSet::new();
    while let Some(r) = value["$ref"].as_str() {
        ensure!(
            r.starts_with("#/") && seen.insert(r),
            "invalid/cyclic response reference"
        );
        value = document
            .pointer(&r[1..])
            .context("missing response reference")?;
    }
    Ok(value.clone())
}

fn private_headers() -> Value {
    json!({"Cache-Control":{"required":true,"schema":{"const":"no-store"}},"Referrer-Policy":{"required":true,"schema":{"const":"no-referrer"}}})
}

pub(super) fn apply(document: &mut Value) -> Result<()> {
    let schemas = document["components"]["schemas"].as_object_mut().unwrap();
    for (code, message) in [
        (401, "Authentication required"),
        (403, "Request verification failed"),
        (500, "Authentication service unavailable"),
    ] {
        schemas.insert(format!("ManagementAuth{code}"),json!({"type":"object","required":["error"],"additionalProperties":false,"properties":{"error":{"const":message}}}));
    }
    schemas.insert("ManagementAuthThrottle".into(),json!({"type":"object","required":["error"],"additionalProperties":false,"properties":{"error":{"const":"Too many sign-in attempts; try again shortly"}}}));
    document["x-omg-middleware-contracts"] = json!({
        "management":{"authentication":"exclusive credential family; duplicate/mixed-header refusal and live session/token checks (401); unsafe browser Origin/CSRF (403); bearer per-IP/replica/per-token throttles before policy (429); authentication storage (500)","authorization":"live role, token policy/scope/IP and workspace/key boundary; storage/audit refusal (503); readiness (501 only if Scope is not token-ready)","body_limit":"413 text/plain before handler extraction; upload routers use their own caps"},
        "logout":{"authentication":"own session/Origin/CSRF verification (401/403/500); not management bearer middleware, no bearer throttle","body_limit":"413 text/plain before handler extraction"},
        "public":{"authentication":"anonymous handler; its own identity-flow statuses remain fragment-owned","body_limit":"413 text/plain before handler extraction"}
    });
    let snapshot = document.clone();
    let mut responses = serde_json::Map::new();
    let mut response_names = std::collections::BTreeMap::<Vec<u8>, String>::new();
    let mut variants = std::collections::BTreeMap::<u16, usize>::new();
    // Explicit order is independent of serde_json/preserve_order feature
    // unification, just like the artifact encoder itself.
    let mut paths = document["paths"]
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect::<Vec<_>>();
    paths.sort();
    for path in paths {
        let item = &mut document["paths"][&path];
        let mut methods = item
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        methods.sort();
        for method in methods {
            let op = &mut item[&method];
            let access = op["x-omg-access"].as_str().unwrap().to_owned();
            let managed = !matches!(access.as_str(), "Anonymous" | "BrowserLogout");
            let logout = access == "BrowserLogout";
            // These constants are serving-layer behavior, not a hand-copied
            // operation inventory. Newly completed areas inherit them directly.
            let mut additions = vec![(413, json!({"type":"string"}), false)];
            if managed || logout {
                for code in [401, 403, 500] {
                    additions.push((
                        code,
                        json!({"$ref":format!("#/components/schemas/ManagementAuth{code}")}),
                        true,
                    ));
                }
            }
            if managed {
                additions.push((
                    429,
                    json!({"$ref":"#/components/schemas/ManagementAuthThrottle"}),
                    true,
                ));
                additions.push((
                    503,
                    json!({"$ref":"#/components/schemas/ErrorEnvelope"}),
                    false,
                ));
                if access.starts_with("Workspace") {
                    additions.push((
                        404,
                        json!({"$ref":"#/components/schemas/ErrorEnvelope"}),
                        false,
                    ));
                }
                if op["x-omg-token-policy"] == "Scope"
                    && !crate::management::authorization::TOKEN_READY_ROUTES
                        .iter()
                        .any(|(m, p)| *m == method.to_ascii_uppercase() && *p == path)
                {
                    additions.push((
                        501,
                        json!({"$ref":"#/components/schemas/ErrorEnvelope"}),
                        false,
                    ));
                }
            }
            for (code, schema, private) in additions {
                let body_schema = if managed && code == 403 {
                    json!({"anyOf":[schema,{"$ref":"#/components/schemas/ErrorEnvelope"}]})
                } else {
                    schema.clone()
                };
                let key = code.to_string();
                let media = if code == 413 {
                    "text/plain"
                } else {
                    "application/json"
                };
                let original = op["responses"]
                    .get(&key)
                    .map(|r| reference(&snapshot, r))
                    .transpose()?;
                let mut response = original.unwrap_or_else(||json!({"description":format!("Serving-layer response {code}; see x-omg-middleware-responses") }));
                let content = &mut response["content"][media];
                if let Some(existing) = content.get("schema") {
                    if existing != &body_schema {
                        content["schema"] = json!({"anyOf":[existing,body_schema]});
                    }
                } else {
                    content["schema"] = body_schema;
                }
                // OpenAPI cannot condition required headers on a JSON body
                // alternative. Keep the standard Header Objects and explicitly
                // bind the identity-only alternatives to required wire headers.
                if private {
                    let mut required = private_headers();
                    if code == 429 {
                        required["Retry-After"] = json!({"required":true,"schema":{"type":"string","pattern":"^[1-9][0-9]*$"}});
                    }
                    for (name, header) in required.as_object().unwrap() {
                        let mut optional = header.clone();
                        optional.as_object_mut().unwrap().remove("required");
                        response["headers"][name] = optional;
                    }
                    response["x-omg-body-header-requirements"] =
                        json!([{"schema":schema,"headers":required}]);
                }
                // Standard OpenAPI Response references keep the committed
                // artifact small and make shared refusals independently visible.
                let response_key = super::generator::encode(&response);
                let name = if let Some(name) = response_names.get(&response_key) {
                    name.clone()
                } else {
                    let variant = variants.entry(code).or_default();
                    *variant += 1;
                    let name = format!("ManagementMiddleware{code}Variant{variant}");
                    response_names.insert(response_key, name.clone());
                    responses.insert(name.clone(), response);
                    name
                };
                op["responses"][&key] = json!({"$ref":format!("#/components/responses/{name}")});
            }
            let contract = if managed {
                "management"
            } else if logout {
                "logout"
            } else {
                "public"
            };
            op["x-omg-middleware-responses"] =
                json!({"$ref":format!("#/x-omg-middleware-contracts/{contract}")});
        }
    }
    document["components"]["responses"] = Value::Object(responses);
    Ok(())
}
