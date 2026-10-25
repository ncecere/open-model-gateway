//! Offline validation: all trusted external identifiers resolve only to checked-in,
//! checksum-verified bytes. Contract references themselves must be local pointers.
use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result, ensure};
use jsonschema::{Draft, PatternOptions, Retrieve, Uri, Validator};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::generator::root;

const OPENAPI_META: &str = "https://spec.openapis.org/oas/3.1/schema-base/2022-10-07";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    files: Vec<Vendored>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Vendored {
    file: String,
    uri: String,
    sha256: String,
}
fn manifest() -> Result<Manifest> {
    Ok(serde_json::from_slice(&std::fs::read(
        root().join("docs/openapi/vendor/manifest.json"),
    )?)?)
}
pub(super) fn vendor_files() -> Result<BTreeMap<String, Vec<u8>>> {
    let mut files = BTreeMap::new();
    for entry in std::fs::read_dir(root().join("docs/openapi/vendor"))? {
        let path = entry?.path();
        let name = path
            .file_name()
            .context("vendor filename missing")?
            .to_str()
            .context("non-UTF8 vendor filename")?;
        if path.extension().is_some_and(|ext| ext == "json") && name != "manifest.json" {
            files.insert(name.to_owned(), std::fs::read(&path)?);
        }
    }
    verify_vendor(&files)?;
    Ok(files)
}
pub(super) fn verify_vendor(files: &BTreeMap<String, Vec<u8>>) -> Result<()> {
    let entries = manifest()?.files;
    let expected = entries
        .iter()
        .map(|e| e.file.clone())
        .collect::<BTreeSet<_>>();
    ensure!(
        entries.len() == expected.len(),
        "duplicate vendored filename"
    );
    ensure!(
        expected.iter().eq(files.keys()),
        "missing or extra vendored schemas"
    );
    let mut uris = BTreeSet::new();
    for entry in entries {
        ensure!(
            !entry.file.contains('/') && !entry.file.contains('\\'),
            "vendor filename must be local"
        );
        ensure!(uris.insert(entry.uri), "duplicate vendor URI");
        let checksum = hex::encode(Sha256::digest(&files[&entry.file]));
        ensure!(
            checksum == entry.sha256,
            "vendored checksum mismatch: {}",
            entry.file
        );
    }
    Ok(())
}
struct LocalSchemas(BTreeMap<String, Value>);
impl Retrieve for LocalSchemas {
    fn retrieve(
        &self,
        uri: &Uri<String>,
    ) -> std::result::Result<Value, Box<dyn std::error::Error + Send + Sync>> {
        self.0
            .get(uri.as_str())
            .cloned()
            .ok_or_else(|| format!("non-vendored reference refused: {uri}").into())
    }
}
fn local_schemas() -> Result<LocalSchemas> {
    let files = vendor_files()?;
    let mut schemas = BTreeMap::new();
    for entry in manifest()?.files {
        let schema: Value = serde_json::from_slice(&files[&entry.file])?;
        ensure!(
            schema["$id"] == entry.uri,
            "vendor id differs from approved retrieval URI"
        );
        schemas.insert(entry.uri, schema);
    }
    Ok(LocalSchemas(schemas))
}

pub(super) fn compile_schema(schema: &Value) -> Result<Validator> {
    // No runtime backtracking: look-around/backreferences, malformed patterns and
    // compilation exhaustion fail at construction. The RED tests cover both regex
    // keywords, including underneath `not` (where treating errors as false is unsafe).
    jsonschema::options()
        .with_draft(Draft::Draft202012)
        .with_retriever(local_schemas()?)
        .with_pattern_options(PatternOptions::regex().size_limit(1 << 20))
        .build(schema)
        .map_err(|error| anyhow::anyhow!("schema compilation failed: {error}"))
}

fn local_references(value: &Value, document: &Value) -> Result<()> {
    match value {
        Value::Object(map) => {
            for keyword in ["$ref", "$dynamicRef", "$recursiveRef"] {
                if let Some(reference) = map.get(keyword) {
                    let reference = reference.as_str().context("reference must be a string")?;
                    ensure!(
                        reference.starts_with("#/"),
                        "only local JSON Pointer references are allowed: {reference}"
                    );
                    ensure!(
                        document.pointer(&reference[1..]).is_some(),
                        "unresolved reference: {reference}"
                    );
                }
            }
            // Do not let a local reference change meaning under a nested base URI.
            ensure!(
                !map.contains_key("$id"),
                "contract schemas must not rebase local references with $id"
            );
            for child in map.values() {
                local_references(child, document)?;
            }
        }
        Value::Array(array) => {
            for child in array {
                local_references(child, document)?;
            }
        }
        _ => {}
    }
    Ok(())
}
fn pointer_segment(value: &str) -> String {
    value.replace('~', "~0").replace('/', "~1")
}
/// RFC 6901 section 6: a JSON Pointer used as a URI fragment must
/// percent-encode every byte outside RFC 3986 `fragment` characters
/// (for example the braces of templated path keys such as `{session}`).
fn fragment(pointer: &str) -> String {
    let mut out = String::with_capacity(pointer.len());
    for byte in pointer.bytes() {
        let allowed = byte.is_ascii_alphanumeric() || b"-._~!$&'()*+,;=:@/?".contains(&byte);
        if allowed {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}
fn inline_schema_pointers(value: &Value, pointer: &str, result: &mut BTreeSet<String>) {
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                let child_pointer = format!("{pointer}/{}", pointer_segment(key));
                if key == "schema" {
                    result.insert(child_pointer.clone());
                }
                inline_schema_pointers(child, &child_pointer, result);
            }
        }
        Value::Array(array) => {
            for (i, child) in array.iter().enumerate() {
                inline_schema_pointers(child, &format!("{pointer}/{i}"), result);
            }
        }
        _ => {}
    }
}
/// Response Links name operations by `operationId`/`operationRef`, which the
/// reference checks above do not resolve. None are needed yet, so refuse them
/// outright rather than let a fragment point at a nonexistent operation.
fn no_links(document: &Value) -> Result<()> {
    ensure!(
        document.pointer("/components/links").is_none(),
        "response links are not supported in the management contract"
    );
    for (path, item) in document["paths"].as_object().context("paths missing")? {
        for (method, operation) in item.as_object().into_iter().flatten() {
            for (status, response) in operation["responses"].as_object().into_iter().flatten() {
                ensure!(
                    response.get("links").is_none(),
                    "response links are not supported: {method} {path} {status}"
                );
            }
        }
    }
    Ok(())
}
pub(super) fn validate_document(document: &Value) -> Result<()> {
    local_references(document, document)?;
    no_links(document)?;
    let validator = compile_schema(&json!({"$ref":OPENAPI_META}))?;
    validator
        .validate(document)
        .map_err(|error| anyhow::anyhow!("OpenAPI 3.1 metaschema: {error}"))?;
    ensure!(document["openapi"] == "3.1.0", "expected OpenAPI 3.1.0");
    // The OAS metaschema validates schema keyword shapes, but does not compile
    // patterns/refs in each body schema. Compile every component (even unused
    // fragments) and inline Schema Object, with the full document as pointer root.
    let mut pointers = BTreeSet::new();
    for name in document["components"]["schemas"]
        .as_object()
        .context("schemas missing")?
        .keys()
    {
        pointers.insert(format!("/components/schemas/{}", pointer_segment(name)));
    }
    inline_schema_pointers(document, "", &mut pointers);
    for pointer in pointers {
        let mut schema = document.clone();
        schema["$ref"] = json!(format!("#{}", fragment(&pointer)));
        compile_schema(&schema).with_context(|| format!("compile contract schema at {pointer}"))?;
    }
    Ok(())
}

#[cfg(test)]
mod fragment_tests {
    #[test]
    fn templated_path_pointers_become_valid_uri_fragments() {
        assert_eq!(
            super::fragment("/paths/~1api~1v1~1me~1sessions~1{session}/delete"),
            "/paths/~1api~1v1~1me~1sessions~1%7Bsession%7D/delete"
        );
        assert_eq!(super::fragment("/a b/%/\"/<>"), "/a%20b/%25/%22/%3C%3E");
    }
}
