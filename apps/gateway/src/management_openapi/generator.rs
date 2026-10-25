use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use syn::visit::Visit;

use crate::route_policy::{Access, ROUTES, TokenPolicy};

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Fragment {
    pub access: String,
    pub token_policy: String,
    pub scope: Option<String>,
    #[serde(default)]
    pub operation: Map<String, Value>,
}

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Fragments {
    #[serde(default, deserialize_with = "unique_map")]
    pub operations: BTreeMap<String, Fragment>,
    #[serde(default, deserialize_with = "unique_map")]
    pub schemas: BTreeMap<String, Value>,
}

// serde's default map decoding silently accepts duplicate keys. Reviewed fragments must not.
fn unique_map<'de, D, T>(deserializer: D) -> std::result::Result<BTreeMap<String, T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    struct Unique<T>(std::marker::PhantomData<T>);
    impl<'de, T: Deserialize<'de>> serde::de::Visitor<'de> for Unique<T> {
        type Value = BTreeMap<String, T>;
        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("an object without duplicate fragment names")
        }
        fn visit_map<A: serde::de::MapAccess<'de>>(
            self,
            mut map: A,
        ) -> std::result::Result<Self::Value, A::Error> {
            let mut result = BTreeMap::new();
            while let Some((key, value)) = map.next_entry::<String, T>()? {
                if result.insert(key.clone(), value).is_some() {
                    return Err(serde::de::Error::custom(format!(
                        "duplicate fragment {key}"
                    )));
                }
            }
            Ok(result)
        }
    }
    deserializer.deserialize_map(Unique(std::marker::PhantomData))
}

pub(super) fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}
pub(super) fn artifact_path() -> PathBuf {
    root().join("docs/openapi/management.openapi.json")
}
pub(super) fn load_fragments() -> Result<Fragments> {
    load_fragments_from(&root().join("docs/openapi/management"))
}
pub(super) fn load_fragments_from(directory: &Path) -> Result<Fragments> {
    let mut paths = std::fs::read_dir(directory)?
        .map(|entry| entry.map(|e| e.path()))
        .collect::<std::io::Result<Vec<_>>>()?;
    paths.sort();
    let mut result = Fragments::default();
    for path in paths {
        if path.extension().is_none_or(|ext| ext != "json") {
            continue;
        }
        let fragment: Fragments = serde_json::from_slice(&std::fs::read(&path)?)
            .with_context(|| format!("fragment {}", path.display()))?;
        for (key, value) in fragment.operations {
            ensure!(
                result.operations.insert(key.clone(), value).is_none(),
                "duplicate operation fragment {key}"
            );
        }
        for (key, value) in fragment.schemas {
            ensure!(
                result.schemas.insert(key.clone(), value).is_none(),
                "duplicate schema fragment {key}"
            );
        }
    }
    Ok(result)
}

struct Declaration {
    method: String,
    path: &'static str,
    access: &'static str,
    token_policy: &'static str,
    scope: Option<&'static str>,
    security: Value,
}
fn access_name(access: Access) -> &'static str {
    match access {
        Access::Anonymous => "Anonymous",
        Access::Health => "Health",
        Access::InferenceKey => "InferenceKey",
        Access::Scim => "Scim",
        Access::Browser => "Browser",
        Access::BrowserLogout => "BrowserLogout",
        Access::PlatformRead => "PlatformRead",
        Access::PlatformWrite => "PlatformWrite",
        Access::WorkspaceMetadata => "WorkspaceMetadata",
        Access::WorkspaceMember => "WorkspaceMember",
        Access::WorkspaceKey => "WorkspaceKey",
        Access::WorkspaceKeyRotate => "WorkspaceKeyRotate",
        Access::WorkspaceAdmin => "WorkspaceAdmin",
        Access::WorkspaceReportAdmin => "WorkspaceReportAdmin",
        Access::WorkspaceAdminOrReader => "WorkspaceAdminOrReader",
        Access::WorkspacePlatformWrite => "WorkspacePlatformWrite",
    }
}
fn inventory() -> Result<BTreeMap<String, Declaration>> {
    let mut inventory = BTreeMap::new();
    for route in ROUTES
        .iter()
        .filter(|route| route.path.starts_with("/api/v1/"))
    {
        for method in route.methods.split_ascii_whitespace() {
            ensure!(
                [
                    "GET", "POST", "PUT", "PATCH", "DELETE", "OPTIONS", "HEAD", "TRACE"
                ]
                .contains(&method),
                "unsupported management method {method}"
            );
            let (token_policy, scope) = match route.token_policy {
                TokenPolicy::Denied => ("Denied", None),
                TokenPolicy::BrowserAdminOnly => ("BrowserAdminOnly", None),
                TokenPolicy::Scope(scope) => ("Scope", Some(scope.as_str())),
            };
            let mut security = Vec::new();
            if route.access != Access::Anonymous {
                let browser = if ["GET", "HEAD", "OPTIONS"].contains(&method) {
                    json!({"browserSession":[]})
                } else {
                    json!({"browserSession":[],"csrfToken":[],"origin":[]})
                };
                security.push(browser);
                if scope.is_some() {
                    security.push(json!({"adminToken":[]}));
                }
            } else {
                ensure!(
                    scope.is_none(),
                    "anonymous operation unexpectedly admits admin tokens"
                );
            }
            let declaration = Declaration {
                method: method.to_ascii_lowercase(),
                path: route.path,
                access: access_name(route.access),
                token_policy,
                scope,
                security: json!(security),
            };
            let key = format!("{method} {}", route.path);
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
            fragment.access == declaration.access
                && fragment.token_policy == declaration.token_policy
                && fragment.scope.as_deref() == declaration.scope,
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
            "x-omg-access":declaration.access,
            "x-omg-token-policy":declaration.token_policy,
            "security":declaration.security,
            "parameters":path_parameters(declaration.path),
            "x-omg-request-schema":{"$ref":format!("#/components/schemas/{request_name}")},
            "x-omg-response-schema":{"$ref":format!("#/components/schemas/{response_name}")},
            "responses":{"default":{"description":"Incomplete response contract; see x-omg-response-schema. Framework extraction errors can differ from the shared ErrorEnvelope."}},
        });
        if let Some(scope) = declaration.scope {
            operation["x-omg-required-scope"] = json!(scope);
        }
        for (field, value) in &fragment.operation {
            ensure!(
                ![
                    "operationId",
                    "security",
                    "x-omg-access",
                    "x-omg-token-policy",
                    "x-omg-required-scope",
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
    schemas.insert("ErrorEnvelope".into(), error_schema()?);
    for (name, schema) in &fragments.schemas {
        ensure!(
            name != "ErrorEnvelope",
            "shared error vocabulary is generated"
        );
        schemas.insert(name.clone(), schema.clone());
    }
    Ok(json!({
        "openapi":"3.1.0",
        "jsonSchemaDialect":"https://spec.openapis.org/oas/3.1/dialect/base",
        "info":{"title":"Open Model Gateway management API","version":env!("CARGO_PKG_VERSION"),
            "description":"INCOMPLETE contract skeleton (#81 PR1). Access extensions are minimum boundaries, not complete authority. Live owner, role, membership, personal-workspace and object-visibility checks remain authoritative. Personal details are owner-only; platform totals do not grant personal detail access. Shared members see their own activity unless an administrator or the object's visibility permits more; ownership checks can hide objects with 404. Auditor platform authority is read-only, independent User/membership entitlements remain usable. Credential families are exclusive, duplicate Authorization headers are refused, inference keys never authorize management, undeclared routes fail closed, and write scopes do not imply read scopes. BrowserAdminOnly includes Platform Admin even on GET. HEAD implicitly follows GET unless separately declared; no synthetic HEAD operations are listed. Request/response placeholders live in x-omg-request-schema and x-omg-response-schema until actual body presence, status and media types are reviewed; they are not permissive acceptance schemas. No universally enveloped error response is promised."},
        "paths":paths,
        "components":{"schemas":schemas,"securitySchemes":{
            "browserSession":{"type":"apiKey","in":"cookie","name":"omg_session","description":"Browser session; live role, workspace and owner checks still apply."},
            "csrfToken":{"type":"apiKey","in":"header","name":"X-CSRF-Token","description":"Conjunctive with session and Origin on browser mutations."},
            "origin":{"type":"apiKey","in":"header","name":"Origin","description":"Must exactly equal the server-configured Origin; conjunctive with session and CSRF on browser mutations."},
            "adminToken":{"type":"http","scheme":"bearer","description":"Admin API token, never an inference key. Only Scope operations admit this alternative; x-omg-required-scope is exact, not an OAuth scope. Live token and owner authority is rechecked."}
        }}
    }))
}

pub(super) fn verify(document: &Value) -> Result<()> {
    let inventory = inventory()?;
    let paths = document["paths"]
        .as_object()
        .context("paths object missing")?;
    let mut actual = BTreeSet::new();
    let mut ids = BTreeSet::new();
    for (path, item) in paths {
        ensure!(path.starts_with("/api/v1/"), "non-management path {path}");
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
                operation["x-omg-access"] == declaration.access
                    && operation["x-omg-token-policy"] == declaration.token_policy
                    && operation
                        .get("x-omg-required-scope")
                        .and_then(Value::as_str)
                        == declaration.scope,
                "access/token/scope drift for {key}"
            );
            ensure!(
                declaration.scope.is_some() || operation.get("x-omg-required-scope").is_none(),
                "scope on non-Scope operation"
            );
            ensure!(
                operation["security"] == declaration.security,
                "security drift for {key}"
            );
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
        }
    }
    ensure!(actual.iter().eq(inventory.keys()), "omitted operations");
    ensure!(
        document["components"]["schemas"]["ErrorEnvelope"] == error_schema()?,
        "undocumented error vocabulary or envelope drift"
    );
    Ok(())
}

// Compiled constants are authoritative for common/policy reasons. Detailed error
// call sites and the manually rendered upload envelope are additionally inventoried
// from parsed source, failing on unrecognized reason expressions rather than guessing.
fn error_reasons() -> Result<BTreeSet<String>> {
    let mut reasons = crate::management::REASONS
        .iter()
        .map(|(_, r)| (*r).to_owned())
        .chain(
            crate::management::POLICY_REASONS
                .iter()
                .map(|(_, r, _, _)| (*r).to_owned()),
        )
        .collect::<BTreeSet<_>>();
    let mut files = Vec::new();
    source_files(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("src/management"),
        &mut files,
    )?;
    for file in files {
        let source = syn::parse_file(&std::fs::read_to_string(&file)?)?;
        let constants = source
            .items
            .iter()
            .filter_map(|item| match item {
                syn::Item::Const(item) => {
                    string_literal(&item.expr).map(|value| (item.ident.to_string(), value))
                }
                _ => None,
            })
            .collect();
        let mut visitor = DetailedReasons {
            constants,
            reasons: &mut reasons,
            errors: Vec::new(),
        };
        visitor.visit_file(&source);
        ensure!(
            visitor.errors.is_empty(),
            "{}: {}",
            file.display(),
            visitor.errors.join("; ")
        );
    }
    Ok(reasons)
}
fn source_files(directory: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    for entry in std::fs::read_dir(directory)? {
        let path = entry?.path();
        let name = path.file_name().unwrap().to_string_lossy();
        if name.contains("test") {
            continue;
        }
        if path.is_dir() {
            source_files(&path, files)?;
        } else if path.extension().is_some_and(|e| e == "rs") {
            files.push(path);
        }
    }
    Ok(())
}
fn string_literal(expr: &syn::Expr) -> Option<String> {
    if let syn::Expr::Lit(syn::ExprLit {
        lit: syn::Lit::Str(s),
        ..
    }) = expr
    {
        Some(s.value())
    } else {
        None
    }
}
struct DetailedReasons<'a> {
    constants: BTreeMap<String, String>,
    reasons: &'a mut BTreeSet<String>,
    errors: Vec<String>,
}
impl<'ast> Visit<'ast> for DetailedReasons<'_> {
    fn visit_item_mod(&mut self, item: &'ast syn::ItemMod) {
        if item
            .attrs
            .iter()
            .any(|attr| attr.path().is_ident("cfg") && quote_cfg_test(attr))
        {
            return;
        }
        syn::visit::visit_item_mod(self, item);
    }
    fn visit_expr_call(&mut self, call: &'ast syn::ExprCall) {
        if let syn::Expr::Path(path) = &*call.func
            && path
                .path
                .segments
                .last()
                .is_some_and(|s| s.ident == "detailed")
        {
            let reason = call.args.iter().nth(1).and_then(|expr| {
                string_literal(expr).or_else(|| {
                    if let syn::Expr::Path(path) = expr {
                        self.constants
                            .get(&path.path.get_ident()?.to_string())
                            .cloned()
                    } else {
                        None
                    }
                })
            });
            if let Some(reason) = reason {
                self.reasons.insert(reason);
            } else {
                self.errors
                    .push("unsupported detailed-error reason; inventory it explicitly".into());
            }
        }
        syn::visit::visit_expr_call(self, call);
    }
    fn visit_item_fn(&mut self, function: &'ast syn::ItemFn) {
        if function.sig.ident == "dashboard_upload_error" {
            struct Upload<'a>(&'a mut BTreeSet<String>);
            impl<'ast> Visit<'ast> for Upload<'_> {
                fn visit_expr_tuple(&mut self, tuple: &'ast syn::ExprTuple) {
                    if tuple.elems.len() == 3
                        && let Some(reason) = string_literal(&tuple.elems[1])
                    {
                        self.0.insert(reason);
                    }
                    syn::visit::visit_expr_tuple(self, tuple);
                }
            }
            Upload(&mut *self.reasons).visit_block(&function.block);
        }
        syn::visit::visit_item_fn(self, function);
    }
}
fn quote_cfg_test(attr: &syn::Attribute) -> bool {
    // Parse the cfg tokens, rather than substring matching comments or identifiers.
    attr.parse_args::<syn::Meta>().is_ok_and(|meta| match meta {
        syn::Meta::Path(path) => path.is_ident("test"),
        syn::Meta::List(list) => list
            .tokens
            .into_iter()
            .any(|token| matches!(token, proc_macro2::TokenTree::Ident(i) if i == "test")),
        _ => false,
    })
}
fn error_schema() -> Result<Value> {
    Ok(
        json!({"type":"object","required":["error"],"additionalProperties":false,
            "description":"Shared management handler envelope, not a universal framework response. code equals the HTTP status string. Selected reasons add period, limit, missing_meters or retry details. Reason-specific details are not yet exhaustive (#81).",
            "properties":{"error":{"type":"object","required":["code","message"],"additionalProperties":true,
                "properties":{
                    "code":{"type":"string","pattern":"^[1-5][0-9]{2}$"},
                    "message":{"type":"string"},"reason":{"type":"string","enum":error_reasons()?},
                    "period":{"type":"string"},"limit":{"type":"string"},
                    "missing_meters":{"type":"array","items":{"type":"string"}},
                    "retry_after_seconds":{"type":"integer","minimum":0}
                }}}
        }),
    )
}
pub(super) fn operation_count(document: &Value) -> usize {
    document["paths"]
        .as_object()
        .unwrap()
        .values()
        .map(|v| v.as_object().unwrap().len())
        .sum()
}
pub(super) fn placeholder_count(value: &Value) -> usize {
    match value {
        Value::Object(map) => {
            usize::from(
                map.get("x-omg-schema-status")
                    .is_some_and(|v| v == "incomplete"),
            ) + map.values().map(placeholder_count).sum::<usize>()
        }
        Value::Array(array) => array.iter().map(placeholder_count).sum(),
        _ => 0,
    }
}
pub(super) fn encode(document: &Value) -> Vec<u8> {
    // Explicit canonical insertion order remains deterministic even if another crate
    // enables serde_json/preserve_order through feature unification.
    fn canonical(value: &Value) -> Value {
        match value {
            Value::Object(map) => Value::Object(
                map.iter()
                    .map(|(k, v)| (k.clone(), canonical(v)))
                    .collect::<BTreeMap<_, _>>()
                    .into_iter()
                    .collect(),
            ),
            Value::Array(array) => Value::Array(array.iter().map(canonical).collect()),
            value => value.clone(),
        }
    }
    let mut bytes = serde_json::to_vec_pretty(&canonical(document)).unwrap();
    bytes.push(b'\n');
    bytes
}
