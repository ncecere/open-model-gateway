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
fn inference_openapi_zero_placeholders_once_pr2_lands() {
    let doc = generated();
    let slots = doc["components"]["schemas"].as_object().unwrap();
    let remaining: Vec<_> = slots
        .iter()
        .filter(|(_, s)| s["x-omg-schema-status"] == "incomplete")
        .map(|(name, _)| name.as_str())
        .collect();
    let expected = [
        "post_v1_chat_completionsRequest",
        "post_v1_chat_completionsResponse",
        "post_v1_messagesRequest",
        "post_v1_messagesResponse",
        "post_v1_responsesRequest",
        "post_v1_responsesResponse",
    ];
    assert!(
        remaining.iter().all(|s| expected.contains(s)),
        "placeholder outside PR2"
    );
    assert_eq!(
        generator::placeholder_count(&doc),
        6,
        "falling baseline: PR2 must remove these six slots to reach zero"
    );
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
        if let syn::Expr::Block(block) = expr {
            if let Some(syn::Stmt::Expr(last, None)) = block.block.stmts.last() {
                return tail(last);
            }
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
                .is_some_and(|s| !s.trim().is_empty())
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
