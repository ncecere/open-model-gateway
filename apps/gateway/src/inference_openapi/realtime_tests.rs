//! Upgrade metadata and PR3 closure controls (#82 PR3c).
use super::{generator, responses::validate_documented, tests::generated};
use axum::http::{HeaderMap, StatusCode};
use serde_json::json;

#[test]
fn inference_openapi_realtime_upgrade_headers() {
    let doc = generated();
    let mut headers = HeaderMap::new();
    for (key, value) in [
        ("upgrade", "websocket"),
        ("connection", "upgrade"),
        ("sec-websocket-accept", "dGhlIHNhbXBsZSBub25jZQ=="),
        ("sec-websocket-protocol", "realtime"),
        ("cache-control", "no-store"),
        ("x-request-id", "01900000-0000-7000-8000-000000000000"),
    ] {
        headers.insert(key, value.parse().unwrap());
    }
    assert!(
        validate_documented(
            &doc,
            "GET",
            "/v1/realtime",
            StatusCode::SWITCHING_PROTOCOLS,
            &headers,
            b""
        )
        .is_ok(),
        "documented upgrade rejected"
    );
    for key in ["upgrade", "connection", "sec-websocket-accept"] {
        let mut bad = headers.clone();
        bad.remove(key);
        assert!(
            validate_documented(
                &doc,
                "GET",
                "/v1/realtime",
                StatusCode::SWITCHING_PROTOCOLS,
                &bad,
                b""
            )
            .is_err(),
            "missing upgrade header accepted"
        );
    }
}

#[test]
fn inference_openapi_realtime_never_offers_x_api_key() {
    let doc = generated();
    assert_eq!(
        doc["paths"]["/v1/realtime"]["get"]["x-omg-websocket"]["auth"],
        json!(["Bearer", "openai-insecure-api-key.<key>"])
    );
    let mut bad = doc.clone();
    bad["paths"]["/v1/realtime"]["get"]["security"] = json!([{"anthropicApiKey":[]}]);
    assert!(
        generator::verify(&bad).is_err(),
        "Realtime x-api-key offered"
    );
}

#[test]
fn inference_openapi_closure_has_zero_placeholders() {
    // #82 closure: PR1, PR2 and PR3 together complete every operation.
    let doc = generated();
    let remaining: Vec<_> = doc["components"]["schemas"]
        .as_object()
        .unwrap()
        .iter()
        .filter(|(_, s)| s["x-omg-schema-status"] == "incomplete")
        .map(|(name, _)| name.as_str())
        .collect();
    assert!(remaining.is_empty(), "placeholders remain: {remaining:?}");
    assert_eq!(generator::placeholder_count(&doc), 0, "closure baseline");
}

#[cfg(feature = "integration-tests")]
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn inference_openapi_realtime_wire_fixtures(pool: sqlx::PgPool) {
    let mut fixtures = super::responses::Fixtures::default();
    crate::protocols::realtime::inference_openapi_handshake_fixtures(pool, &mut fixtures).await;
    fixtures.finish(super::responses::REALTIME_AREAS);
}

/// Read the actual `let (close, ..) = match end` dispatch in the handler,
/// rather than maintaining a second list of WebSocket status numbers here.
fn handler_close_codes() -> std::collections::BTreeSet<u16> {
    use syn::visit::Visit;
    struct Table {
        constants: std::collections::BTreeMap<String, u16>,
        codes: std::collections::BTreeSet<u16>,
        found: usize,
    }
    fn tail(expr: &syn::Expr) -> &syn::Expr {
        if let syn::Expr::Block(block) = expr
            && let Some(syn::Stmt::Expr(last, None)) = block.block.stmts.last()
        {
            return tail(last);
        }
        expr
    }
    impl<'ast> Visit<'ast> for Table {
        fn visit_local(&mut self, local: &'ast syn::Local) {
            if let syn::Pat::Tuple(pattern) = &local.pat
                && matches!(pattern.elems.first(),Some(syn::Pat::Ident(name)) if name.ident=="close")
            {
                let init = local
                    .init
                    .as_ref()
                    .expect("close table initializer missing");
                let syn::Expr::Match(table) = tail(&init.expr) else {
                    panic!("close table is no longer a match")
                };
                assert!(
                    matches!(&*table.expr,syn::Expr::Path(path) if path.path.is_ident("end")),
                    "close table selector changed"
                );
                self.found += 1;
                for arm in &table.arms {
                    let syn::Expr::Tuple(result) = tail(&arm.body) else {
                        panic!("close table arm shape changed")
                    };
                    let first = result.elems.first().expect("close table arm is empty");
                    if matches!(first,syn::Expr::Path(path) if path.path.is_ident("None")) {
                        continue;
                    }
                    let syn::Expr::Call(some) = first else {
                        panic!("close table optional code shape changed")
                    };
                    assert!(
                        matches!(&*some.func,syn::Expr::Path(path) if path.path.is_ident("Some")),
                        "close table optional constructor changed"
                    );
                    let Some(syn::Expr::Tuple(close)) = some.args.first() else {
                        panic!("close tuple missing")
                    };
                    let code = match close.elems.first().expect("close code missing") {
                        syn::Expr::Lit(syn::ExprLit {
                            lit: syn::Lit::Int(value),
                            ..
                        }) => value.base10_parse().expect("close code is not u16"),
                        syn::Expr::Path(path) => *self
                            .constants
                            .get(
                                &path
                                    .path
                                    .get_ident()
                                    .expect("close constant path changed")
                                    .to_string(),
                            )
                            .expect("close constant is not a local integer"),
                        _ => panic!("close code expression requires review"),
                    };
                    self.codes.insert(code);
                }
            }
            syn::visit::visit_local(self, local);
        }
    }
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/protocols/realtime.rs");
    let file = syn::parse_file(&std::fs::read_to_string(path).unwrap()).unwrap();
    let constants = file
        .items
        .iter()
        .filter_map(|item| {
            if let syn::Item::Const(item) = item
                && let syn::Expr::Lit(syn::ExprLit {
                    lit: syn::Lit::Int(value),
                    ..
                }) = &*item.expr
            {
                return Some((
                    item.ident.to_string(),
                    value.base10_parse().expect("integer constant outside u16"),
                ));
            }
            None
        })
        .collect();
    let proxy = file
        .items
        .iter()
        .find_map(|item| match item {
            syn::Item::Fn(f) if f.sig.ident == "proxy" => Some(f),
            _ => None,
        })
        .expect("realtime proxy missing");
    let mut table = Table {
        constants,
        codes: Default::default(),
        found: 0,
    };
    table.visit_block(&proxy.block);
    assert!(
        table.found == 1 && !table.codes.is_empty(),
        "realtime close dispatch table missing or ambiguous"
    );
    table.codes
}

// Reviewed prose is deliberately exact: a nonempty but partial trigger
// description must not erase a close path. The numeric inventory itself is
// still derived exclusively from the handler dispatch above.
fn reviewed_close_trigger(code: u16) -> Option<&'static str> {
    match code {
        1000 => {
            Some("Idle timeout or maximum session duration, with or without an in-flight response.")
        }
        1002 => Some(
            "The client violated the WebSocket protocol, such as a continuation frame with nothing to continue (protocol_error).",
        ),
        1003 => Some("A binary client frame is not supported; send JSON text events."),
        1007 => Some("A client text frame or close reason is not valid UTF-8 (invalid_payload)."),
        1008 => {
            Some("Rejected client event, event-rate limit, or live authorization/admission denial.")
        }
        1009 => Some(
            "The client WebSocket frame or assembled message exceeds the configured transport size cap (message_too_big).",
        ),
        1011 => {
            Some("Fatal upstream/server failure, including invalid or unaccounted upstream events.")
        }
        _ => None,
    }
}

fn close_metadata_matches(
    doc: &serde_json::Value,
    codes: &std::collections::BTreeSet<u16>,
) -> bool {
    let ws = &doc["paths"]["/v1/realtime"]["get"]["x-omg-websocket"];
    let Some(list) = ws["close_codes"].as_array() else {
        return false;
    };
    let documented: Option<std::collections::BTreeSet<u16>> = list
        .iter()
        .map(|v| v.as_u64().and_then(|n| u16::try_from(n).ok()))
        .collect();
    if documented.as_ref() != Some(codes) || list.len() != codes.len() {
        return false;
    }
    let Some(triggers) = ws["close_code_triggers"].as_object() else {
        return false;
    };
    triggers.len() == codes.len()
        && codes.iter().all(|code| {
            triggers
                .get(&code.to_string())
                .and_then(serde_json::Value::as_str)
                .is_some_and(|s| reviewed_close_trigger(*code) == Some(s))
        })
}

#[test]
fn inference_openapi_review_close_codes_match_handler_dispatch() {
    let codes = handler_close_codes();
    let doc = generated();
    assert!(
        close_metadata_matches(&doc, &codes),
        "documented close codes or triggers differ from handler dispatch"
    );
    for code in &codes {
        let mut bad = doc.clone();
        bad["paths"]["/v1/realtime"]["get"]["x-omg-websocket"]["close_codes"]
            .as_array_mut()
            .unwrap()
            .retain(|v| v.as_u64() != Some(u64::from(*code)));
        assert!(
            !close_metadata_matches(&bad, &codes),
            "missing handler close code escaped the pin"
        );
    }
    for code in &codes {
        let mut deleted = doc.clone();
        deleted["paths"]["/v1/realtime"]["get"]["x-omg-websocket"]["close_code_triggers"]
            .as_object_mut()
            .unwrap()
            .remove(&code.to_string());
        assert!(
            !close_metadata_matches(&deleted, &codes),
            "missing close trigger escaped the pin"
        );
        for replacement in ["", "x"] {
            let mut bad = doc.clone();
            bad["paths"]["/v1/realtime"]["get"]["x-omg-websocket"]["close_code_triggers"]
                [code.to_string()] = json!(replacement);
            assert!(
                !close_metadata_matches(&bad, &codes),
                "blank or unreviewed close trigger escaped the pin"
            );
        }
    }
    let mut partial = doc.clone();
    partial["paths"]["/v1/realtime"]["get"]["x-omg-websocket"]["close_code_triggers"]["1000"] =
        json!("Idle timeout");
    assert!(
        !close_metadata_matches(&partial, &codes),
        "partial idle trigger erased session-duration or in-flight coverage"
    );
    let mut bad = doc;
    bad["paths"]["/v1/realtime"]["get"]["x-omg-websocket"]["close_codes"]
        .as_array_mut()
        .unwrap()
        .push(json!(4999));
    assert!(
        !close_metadata_matches(&bad, &codes),
        "invented close code escaped the pin"
    );
}
