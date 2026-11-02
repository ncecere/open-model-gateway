//! Generator and drift gates of `docs/openapi/inference.openapi.json` (#82).
//!
//! Paths, methods, path parameters, operation IDs, access/token extensions and
//! security requirements are generated from the compiled `/v1/*` entries of
//! `route_policy::ROUTES`; request/response schemas are reviewed fragments in
//! `docs/openapi/inference/*.json` (the #81 fragment format and loader). The
//! shared error envelopes, vocabulary and reusable responses are generated from
//! source by `errors.rs` and cannot be overridden by a fragment.
//!
//! This is a deliberate COPY of the management generator's structure, not a
//! parameterisation of it: its inventory, security model, document shell and
//! drift checks are management-specific (browser sessions, scopes, `/api/v1/`,
//! one `ErrorEnvelope`), so reusing it would have meant changing management
//! behaviour. The fragment types, loader, encoder and counters ARE shared.
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
};

use anyhow::{Context, Result, ensure};
use serde_json::{Map, Value, json};

use crate::{
    management::openapi::generator::{self as shared, Fragments},
    route_policy::{Access, ROUTES, TokenPolicy},
};

pub(super) const PREFIX: &str = "/v1/";

pub(super) fn artifact_path() -> PathBuf {
    shared::root().join("docs/openapi/inference.openapi.json")
}
pub(super) fn fragments_dir() -> PathBuf {
    shared::root().join("docs/openapi/inference")
}
pub(super) fn load_fragments() -> Result<Fragments> {
    shared::load_fragments_from(&fragments_dir())
}

/// Credential alternatives beyond the Bearer inference key, by path. Reviewed
/// table: `wiring_tests.rs` proves it against the real credential parsers
/// (`http::credential` for Messages, `protocols::realtime::credential`).
const EXTRA_CREDENTIALS: &[(&str, &str)] = &[
    ("/v1/messages", "anthropicApiKey"),
    ("/v1/realtime", "realtimeSubprotocol"),
];
/// Non-path parameters a route reads (`protocols/realtime.rs` `model_param`).
/// Everything else is ignored by the real handlers, so none may be invented.
pub(super) const HEADER_PARAMETERS: &[(&str, &str, &str)] = &[
    ("POST", "/v1/messages", "anthropic-version"),
    ("POST", "/v1/messages", "anthropic-beta"),
];

pub(super) const QUERY_PARAMETERS: &[(&str, &str, &str)] = &[
    ("GET", "/v1/realtime", "model"),
    ("GET", "/v1/files", "purpose"),
    ("GET", "/v1/files", "limit"),
    ("GET", "/v1/files", "after"),
    ("GET", "/v1/files", "order"),
    ("GET", "/v1/batches", "limit"),
    ("GET", "/v1/batches", "after"),
    ("GET", "/v1/batches", "order"),
    ("GET", "/v1/videos", "limit"),
    ("GET", "/v1/videos", "after"),
    ("GET", "/v1/videos", "order"),
    ("GET", "/v1/videos/{id}/content", "variant"),
];

struct Declaration {
    method: String,
    path: &'static str,
    security: Value,
}

fn security(path: &str) -> Value {
    let mut alternatives = vec![json!({"inferenceKey":[]})];
    for (_, scheme) in EXTRA_CREDENTIALS.iter().filter(|(p, _)| *p == path) {
        let mut alternative = Map::new();
        alternative.insert((*scheme).to_owned(), json!([]));
        alternatives.push(Value::Object(alternative));
    }
    json!(alternatives)
}

/// Reviewed PR2 route -> client protocol join. Other areas retain their PR1/
/// PR3 metadata scope; this does not infer provider support from a route name.
fn generation_profiles(path: &str) -> Option<Value> {
    use crate::inference::types::ApiProtocol as P;
    let protocol = match path {
        "/v1/chat/completions" => P::ChatCompletions,
        "/v1/responses" => P::Responses,
        "/v1/messages" => P::Messages,
        _ => return None,
    };
    Some(json!(
        crate::providers::capabilities::PROFILES
            .iter()
            .filter(|(_, protocols)| protocols.contains(&protocol))
            .map(|(name, _)| *name)
            .collect::<Vec<_>>()
    ))
}

fn inventory() -> Result<BTreeMap<String, Declaration>> {
    let mut inventory = BTreeMap::new();
    for route in ROUTES.iter().filter(|route| route.path.starts_with(PREFIX)) {
        ensure!(
            route.access == Access::InferenceKey && route.token_policy == TokenPolicy::Denied,
            "a /v1 route is not an inference-key route that denies admin tokens: review the contract"
        );
        for method in route.methods.split_ascii_whitespace() {
            ensure!(
                ["GET", "POST", "PUT", "PATCH", "DELETE"].contains(&method),
                "unsupported inference method {method}"
            );
            let key = format!("{method} {}", route.path);
            let declaration = Declaration {
                method: method.to_ascii_lowercase(),
                path: route.path,
                security: security(route.path),
            };
            ensure!(
                inventory.insert(key.clone(), declaration).is_none(),
                "duplicate route {key}"
            );
        }
    }
    Ok(inventory)
}

fn operation_id(declaration: &Declaration) -> String {
    let mut id = declaration.method.clone();
    for segment in declaration.path.split('/').filter(|s| !s.is_empty()) {
        id.push('_');
        if segment.starts_with('{') {
            id.push_str("by_");
        }
        id.push_str(&segment.trim_matches(['{', '}']).replace('-', "_"));
    }
    id
}
fn path_parameters(path: &str) -> Vec<Value> {
    path.split('/')
        .filter_map(|segment| segment.strip_prefix('{')?.strip_suffix('}'))
        .map(|name| json!({"name":name,"in":"path","required":true,"schema":{"type":"string"}}))
        .collect()
}
fn placeholder(description: &str) -> Value {
    json!({"description":description,"x-omg-schema-status":"incomplete"})
}

pub(super) fn generate(fragments: &Fragments) -> Result<Value> {
    let inventory = inventory()?;
    ensure!(
        inventory.keys().eq(fragments.operations.keys()),
        "missing or unmatched operation fragments (exact METHOD template required)"
    );
    let mut paths = Map::new();
    let mut schemas = Map::new();
    let mut ids = BTreeSet::new();
    for (key, declaration) in &inventory {
        let fragment = &fragments.operations[key];
        ensure!(
            fragment.access == "InferenceKey"
                && fragment.token_policy == "Denied"
                && fragment.scope.is_none(),
            "authority fragment drift for {key}"
        );
        let id = operation_id(declaration);
        ensure!(ids.insert(id.clone()), "duplicate operationId {id}");
        let request_name = format!("{id}Request");
        let response_name = format!("{id}Response");
        schemas.insert(request_name.clone(), placeholder("Request body presence, media types and schema have not been inventoried. This is not a promise that a body is accepted."));
        schemas.insert(response_name.clone(), placeholder("Response statuses, headers, media types and body schemas have not been inventoried. This is not a promise of a JSON response."));
        let mut operation = json!({
            "operationId":id,
            "x-omg-access":"InferenceKey",
            "x-omg-token-policy":"Denied",
            "security":declaration.security,
            "parameters":path_parameters(declaration.path),
            "x-omg-request-schema":{"$ref":format!("#/components/schemas/{request_name}")},
            "x-omg-response-schema":{"$ref":format!("#/components/schemas/{response_name}")},
            "responses":{"default":{"description":"Incomplete response contract; see x-omg-response-schema."}},
        });
        if let Some(profiles) = generation_profiles(declaration.path) {
            operation["x-omg-protocol-profiles"] = profiles;
        }
        for (field, value) in &fragment.operation {
            ensure!(
                ![
                    "operationId",
                    "security",
                    "x-omg-access",
                    "x-omg-token-policy",
                    "x-omg-required-scope",
                    "x-omg-protocol-profiles",
                    "x-omg-request-schema",
                    "x-omg-response-schema"
                ]
                .contains(&field.as_str()),
                "fragment overrides generated {field} for {key}"
            );
            if field == "parameters" {
                let parameters = value
                    .as_array()
                    .context("fragment parameters must be an array")?;
                ensure!(
                    parameters
                        .iter()
                        .all(|p| p["in"] != "path" && p.get("$ref").is_none()),
                    "path parameters are generated; fragments may add only inline non-path parameters"
                );
                operation["parameters"]
                    .as_array_mut()
                    .unwrap()
                    .extend(parameters.iter().cloned());
            } else {
                operation[field] = value.clone();
            }
        }
        paths.entry(declaration.path).or_insert_with(|| json!({}))[&declaration.method] = operation;
    }
    let shared_components = super::errors::components()?;
    for (name, schema) in shared_components["schemas"].as_object().unwrap() {
        schemas.insert(name.clone(), schema.clone());
    }
    for (name, schema) in &fragments.schemas {
        ensure!(
            !shared_components["schemas"]
                .as_object()
                .unwrap()
                .contains_key(name),
            "the shared error vocabulary and envelopes are generated; {name} cannot be overridden"
        );
        schemas.insert(name.clone(), schema.clone());
    }
    Ok(json!({
        "openapi":"3.1.0",
        "x-omg-integer-encoding":"json-integer-literal",
        "jsonSchemaDialect":"https://spec.openapis.org/oas/3.1/dialect/base",
        "info":{"title":"Open Model Gateway inference API","version":env!("CARGO_PKG_VERSION"),
            "description":format!("Complete contract (#82): every operation (models, embeddings, rerank, System One, images, Chat Completions, Responses, Messages, audio, files, batches, videos and realtime) documents its real HTTP, streaming and media contract and is wire-validated against the real handlers; no placeholder remains. Every operation authenticates with an inference key only (Bearer; /v1/messages also accepts x-api-key, GET /v1/realtime also the openai-insecure-api-key.<key> WebSocket subprotocol, never both); inference keys never authorize management and admin tokens never authorize inference. Live key, workspace, catalog and budget checks remain authoritative. The OpenAI-shaped, Anthropic-shaped, job and file error envelopes are distinct schemas and never the management ErrorEnvelope; /v1/messages errors carry no gateway machine code. x-omg-* extensions state what OpenAPI 3.1 cannot express (see docs/openapi/inference/README.md). {}", crate::management::openapi::quality::INTEGER_ENCODING_DESCRIPTION)},
        "paths":paths,
        "components":{
            "schemas":schemas,
            "headers":shared_components["headers"],
            "responses":shared_components["responses"],
            "securitySchemes":{
                "inferenceKey":{"type":"http","scheme":"bearer","description":"Inference API key (omg_...) as `Authorization: Bearer`. Belongs to one workspace; tenant and workspace are derived from it, never from the request. Never a management credential."},
                "anthropicApiKey":{"type":"apiKey","in":"header","name":"x-api-key","description":"POST /v1/messages only: the inference key in `x-api-key`, as an alternative to Bearer. Sending both, or two of either, is refused with 401."},
                "realtimeSubprotocol":{"type":"apiKey","in":"header","name":"Sec-WebSocket-Protocol","description":"GET /v1/realtime only: the inference key as the `openai-insecure-api-key.<key>` WebSocket subprotocol (beside `realtime`), as an alternative to Bearer, never together with it. x-api-key is not accepted."}
            }
        }
    }))
}

/// Compare a document against what the compiled routes and sources dictate.
pub(super) fn verify(document: &Value) -> Result<()> {
    let inventory = inventory()?;
    let paths = document["paths"]
        .as_object()
        .context("paths object missing")?;
    let mut actual = BTreeSet::new();
    let mut ids = BTreeSet::new();
    for (path, item) in paths {
        ensure!(path.starts_with(PREFIX), "non-inference path {path}");
        for (method, operation) in item.as_object().context("invalid path item")? {
            let key = format!("{} {path}", method.to_ascii_uppercase());
            let declaration = inventory
                .get(&key)
                .with_context(|| format!("extra operation {key}"))?;
            ensure!(
                method == &declaration.method,
                "noncanonical method {method}"
            );
            actual.insert(key.clone());
            ensure!(
                operation["operationId"] == operation_id(declaration),
                "operationId drift for {key}"
            );
            ensure!(
                ids.insert(
                    operation["operationId"]
                        .as_str()
                        .context("operationId missing")?
                ),
                "duplicate operationId"
            );
            ensure!(
                operation["x-omg-access"] == "InferenceKey"
                    && operation["x-omg-token-policy"] == "Denied"
                    && operation.get("x-omg-required-scope").is_none(),
                "access/token/scope drift for {key}"
            );
            ensure!(
                operation["security"] == declaration.security,
                "security drift for {key}"
            );
            if let Some(profiles) = generation_profiles(path) {
                ensure!(
                    operation["x-omg-protocol-profiles"] == profiles,
                    "generation protocol-profile drift for {key}"
                );
            }
            let parameters = operation["parameters"]
                .as_array()
                .context("parameters missing")?;
            let actual_path = parameters
                .iter()
                .filter(|p| p["in"] == "path")
                .cloned()
                .collect::<Vec<_>>();
            ensure!(
                actual_path == path_parameters(path),
                "path parameters drift for {key}"
            );
            // Only parameters a real handler reads may be documented.
            let documented: BTreeSet<(&str, &str)> = parameters
                .iter()
                .filter(|p| p["in"] != "path")
                .filter_map(|p| Some((p["in"].as_str()?, p["name"].as_str()?)))
                .collect();
            let read: BTreeSet<(&str, &str)> = QUERY_PARAMETERS
                .iter()
                .filter(|(m, p, _)| m.eq_ignore_ascii_case(method) && p == path)
                .map(|(_, _, name)| ("query", *name))
                .chain(
                    HEADER_PARAMETERS
                        .iter()
                        .filter(|(m, p, _)| m.eq_ignore_ascii_case(method) && p == path)
                        .map(|(_, _, name)| ("header", *name)),
                )
                .collect();
            ensure!(
                documented == read
                    && documented.len() == parameters.iter().filter(|p| p["in"] != "path").count(),
                "query/header parameter drift for {key}: document exactly the parameters the handler reads"
            );
        }
    }
    ensure!(actual.iter().eq(inventory.keys()), "omitted operations");
    super::errors::verify_components(document)?;
    let parameters = document["paths"]["/v1/messages"]["post"]["parameters"]
        .as_array()
        .context("Messages protocol headers")?;
    let version = parameters
        .iter()
        .find(|p| p["name"] == "anthropic-version")
        .context("Messages version header")?;
    ensure!(
        version["required"] == true
            && version["schema"]["const"] == "2023-06-01"
            && version["x-omg-max-occurrences"] == 1,
        "Messages version declaration drift"
    );
    let beta = parameters
        .iter()
        .find(|p| p["name"] == "anthropic-beta")
        .context("Messages beta header")?;
    ensure!(
        beta["required"] == false
            && beta["schema"]["type"] == "string"
            && beta["schema"].get("enum").is_none()
            && beta["schema"]["minLength"] == 1
            && beta["schema"]["maxLength"] == 1024
            && beta["x-omg-max-occurrences"] == 16
            && beta["x-omg-max-tokens"] == 32
            && beta["x-omg-denied-prefixes"]
                == json!(crate::providers::messages::DENIED_BETA_PREFIXES),
        "Messages open beta list declaration drift"
    );
    verify_transport_exceptions(document)?;
    Ok(())
}

/// Transport statuses a route answers in a shape of its own, even though the
/// operation is still incomplete: `/v1/messages` rewrites 405 and 413 into
/// Anthropic's shape in the outer layer (`http.rs` request_context,
/// `protocols/messages.rs` http_error), unlike every other route. They must be
/// IN the document, with the Anthropic envelope.
const TRANSPORT_EXCEPTIONS: &[(&str, &str, &[&str])] =
    &[("/v1/messages", "post", &["401", "405", "413"])];

fn verify_transport_exceptions(document: &Value) -> Result<()> {
    for (path, method, statuses) in TRANSPORT_EXCEPTIONS {
        for status in *statuses {
            let response = &document["paths"][*path][*method]["responses"][*status];
            ensure!(
                response["content"]["application/json"]["schema"]
                    == json!({"$ref":"#/components/schemas/AnthropicErrorEnvelope"}),
                "{method} {path} must document its {status} transport exception in the Anthropic envelope"
            );
        }
    }
    ensure!(
        document["paths"]["/v1/messages"]["post"]["responses"]["405"]["headers"]["allow"]["schema"]
            == json!({"const":"POST"}),
        "the /v1/messages 405 must keep its Allow header"
    );
    Ok(())
}

pub(super) fn operation_count(document: &Value) -> usize {
    shared::operation_count(document)
}
pub(super) fn placeholder_count(document: &Value) -> usize {
    shared::placeholder_count(document)
}
pub(super) fn encode(document: &Value) -> Vec<u8> {
    shared::encode(document)
}
