//! Media regressions: binary transport completion is not inferred from bytes.
use super::tests::generated;

#[test]
fn inference_openapi_speech_truncation_is_not_clean() {
    let doc = generated();
    let speech = &doc["paths"]["/v1/audio/speech"]["post"];
    assert_eq!(speech["x-omg-binary-stream"]["late_error"], "truncated");
    assert_eq!(
        speech["x-omg-binary-stream"]["positive_requires"],
        "clean transport EOF"
    );
    assert!(speech["x-omg-cancellation"].is_object());
}

#[test]
fn inference_openapi_transcription_strict_parts() {
    let doc = generated();
    let parts = &doc["paths"]["/v1/audio/transcriptions"]["post"]["x-omg-multipart"];
    assert_eq!(parts["max_parts"], 16);
    assert_eq!(parts["max_header_bytes"], 8192);
    assert_eq!(parts["max_field_bytes"], 4096);
    assert_eq!(parts["unknown_parts"], "reject");
    assert_eq!(parts["file_requires_filename"], true);
    assert_eq!(parts["duplicate_fields"], "reject except stream=false");
    assert_eq!(
        parts["part_headers"],
        serde_json::json!(["Content-Disposition", "Content-Type"])
    );
}

#[cfg(feature = "integration-tests")]
mod fixtures {
    use super::super::{
        fixtures_tests::{gateway, request, serve},
        responses::{self as r, Fixtures, capture, probe},
    };
    use axum::{
        body::{Body, Bytes},
        http::{Request, StatusCode},
    };
    use serde_json::json;
    use sqlx::PgPool;
    use tower::ServiceExt;

    fn multipart(format: &str, extra: &str) -> Vec<u8> {
        let wav = crate::inference::audio::tests_support::wav(850);
        let mut body = format!("--B\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\ncompany/smart\r\n--B\r\nContent-Disposition: form-data; name=\"response_format\"\r\n\r\n{format}\r\n{extra}--B\r\nContent-Disposition: form-data; name=\"file\"; filename=\"audio.wav\"\r\nContent-Type: audio/wav\r\n\r\n").into_bytes();
        body.extend(wav);
        body.extend_from_slice(b"\r\n--B--\r\n");
        body
    }

    #[sqlx::test(migrations = "./enterprise_migrations")]
    async fn inference_openapi_media_wire_fixtures(pool: PgPool) {
        let (app, keys) = gateway(&pool).await;
        let key = &keys.personal_key.token;
        let mut fixtures = Fixtures::default();
        let na = |m: &str| json!({"meter":m,"not_applicable":true});
        let mut lines: Vec<_> = [
            "input_tokens",
            "output_tokens",
            "cache_read_tokens",
            "cache_write_tokens",
            "cache_write_5m_tokens",
            "cache_write_1h_tokens",
            "output_images",
            "output_audio_seconds_ms",
            "search_units",
        ]
        .into_iter()
        .map(na)
        .collect();
        for (meter, batch) in [
            ("input_audio_seconds_ms", 1000),
            ("input_characters", 1000000),
            ("requests", 1),
        ] {
            lines.push(json!({"meter":meter,"microusd_per_batch":"0","batch":batch,"unit_label":crate::billing::v3::Meter::ALL.into_iter().find(|m| m.as_str() == meter).unwrap().unit_label(batch).unwrap(),"sku_label":"fixture"}));
        }
        let deployment: uuid::Uuid = sqlx::query_scalar("SELECT id FROM deployments")
            .fetch_one(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO deployment_prices(id,deployment_id,input_token_limit,output_token_limit,pricing_version,price_lines,max_units) VALUES($1,$2,1000,1000,3,$3,'{}')").bind(uuid::Uuid::new_v4()).bind(deployment).bind(json!(lines)).execute(&pool).await.unwrap();
        serve(&pool, "audio_transcriptions").await;
        for (format, extra) in [
            ("json", ""),
            (
                "json",
                "--B\r\nContent-Disposition: form-data; name=\"language\"\r\n\r\nen\r\n",
            ),
            ("text", ""),
        ] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/v1/audio/transcriptions")
                        .header("authorization", format!("Bearer {key}"))
                        .header("content-type", "multipart/form-data; boundary=B")
                        .body(Body::from(multipart(format, extra)))
                        .unwrap(),
                )
                .await
                .unwrap();
            let captured = capture("POST", "/v1/audio/transcriptions", response).await;
            assert_eq!(captured.status, StatusCode::OK, "transcription status");
            fixtures.observe(&captured);
        }
        // Bind the multipart extension to actual handler refusals, not just
        // a second handwritten copy of its constants.
        for extra in [
            "--B\r\nContent-Disposition: form-data; name=\"unknown\"\r\n\r\nx\r\n",
            "--B\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\nx\r\n",
            "--B\r\nContent-Disposition: form-data; name=\"prompt\"; filename=\"x.txt\"\r\n\r\nx\r\n",
            "--B\r\nContent-Disposition: form-data; name=\"prompt\"\r\nX-Unknown: x\r\n\r\nx\r\n",
        ] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/v1/audio/transcriptions")
                        .header("authorization", format!("Bearer {key}"))
                        .header("content-type", "multipart/form-data; boundary=B")
                        .body(Body::from(multipart("json", extra)))
                        .unwrap(),
                )
                .await
                .unwrap();
            let c = capture("POST", "/v1/audio/transcriptions", response).await;
            assert_eq!(
                c.status,
                StatusCode::BAD_REQUEST,
                "multipart refusal status"
            );
        }
        serve(&pool, "audio_speech").await;
        for format in ["mp3", "wav", "opus", "pcm"] {
            let body = json!({"model":"company/smart","input":"hello","voice":"alloy","response_format":format,"speed":1.0,"stream_format":"audio","instructions":null});
            let positional = json!([
                "company/smart",
                "hello",
                "alloy",
                format,
                1.0,
                null,
                "audio"
            ]);
            for valid in [body, positional] {
                fixtures
                    .body_vectors("POST", "/v1/audio/speech", valid, &[], &[], async |body| {
                        probe(
                            capture(
                                "POST",
                                "/v1/audio/speech",
                                app.clone()
                                    .oneshot(request(
                                        "POST",
                                        "/v1/audio/speech",
                                        Some(key),
                                        Some(&body),
                                    ))
                                    .await
                                    .unwrap(),
                            )
                            .await,
                        )
                    })
                    .await;
            }
        }
        // A body error after a valid prefix is not clean audio. The collector
        // must refuse it before any fixture can observe a success.
        let body = Body::from_stream(futures::stream::iter(vec![
            Ok(Bytes::from_static(b"audio")),
            Err(std::io::Error::other("fixture stream failure")),
        ]));
        let failed = tokio::spawn(async move {
            let mut response = axum::response::Response::new(body);
            response
                .headers_mut()
                .insert("content-type", "audio/mpeg".parse().unwrap());
            response
                .headers_mut()
                .insert("cache-control", "no-store".parse().unwrap());
            response.headers_mut().insert(
                "x-request-id",
                "01900000-0000-7000-8000-000000000000".parse().unwrap(),
            );
            // capture must refuse before returning an observable Captured.
            capture("POST", "/v1/audio/speech", response).await
        })
        .await;
        assert!(
            failed.is_err_and(|error| error.is_panic()),
            "truncated speech returned an observable success"
        );
        fixtures.finish(r::MEDIA_AREAS);
    }
}

#[test]
fn inference_openapi_binary_media_type_must_match() {
    use axum::http::{HeaderMap, StatusCode};
    let mut doc = generated();
    let mut headers = HeaderMap::new();
    headers.insert("content-type", "audio/mpeg".parse().unwrap());
    headers.insert("cache-control", "no-store".parse().unwrap());
    headers.insert(
        "x-request-id",
        "01900000-0000-7000-8000-000000000000".parse().unwrap(),
    );
    doc["paths"]["/v1/audio/speech"]["post"]["responses"]["200"]["content"]["audio/mpeg"]["schema"]
        ["contentMediaType"] = serde_json::json!("audio/wav");
    assert!(
        super::responses::validate_documented(
            &doc,
            "POST",
            "/v1/audio/speech",
            StatusCode::OK,
            &headers,
            b"audio"
        )
        .is_err(),
        "inconsistent binary media accepted"
    );
}

#[test]
fn inference_openapi_speech_cancellation_registry_names_a_real_drop_test() {
    use syn::visit::Visit;
    struct Tests(Vec<String>);
    impl<'ast> Visit<'ast> for Tests {
        fn visit_item_fn(&mut self, function: &'ast syn::ItemFn) {
            if function
                .attrs
                .iter()
                .any(|a| a.path().segments.last().is_some_and(|s| s.ident == "test"))
            {
                self.0.push(function.sig.ident.to_string());
            }
            syn::visit::visit_item_fn(self, function);
        }
    }
    let doc = generated();
    let expected = "dropping_http_speech_body_cancels_upstream_and_keeps_the_hold";
    assert_eq!(
        doc["paths"]["/v1/audio/speech"]["post"]["x-omg-cancellation"]["drop_test"], expected,
        "speech cancellation registry drift"
    );
    let source = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/audio.rs"),
    )
    .unwrap();
    let mut tests = Tests(vec![]);
    tests.visit_file(&syn::parse_file(&source).unwrap());
    assert!(
        tests.0.iter().any(|name| name == expected),
        "registered speech cancellation test missing"
    );
}

/// Controls for the shared number-vector addition: +/-1 would miss both of
/// these slightly looser handlers. Integer-field checks are unchanged.
#[tokio::test]
async fn inference_openapi_number_vectors_detect_adjacent_bound_drift() {
    use crate::management::openapi::{
        http_vectors::{Probe, run_body_with},
        responses::Captured,
    };
    use axum::{
        body::Bytes,
        http::{HeaderMap, StatusCode},
    };
    use serde_json::{Value, json};
    let doc = json!({"paths":{"/v1/speed":{"post":{"requestBody":{"required":true,"content":{"application/json":{"schema":{"type":"object","required":["n"],"additionalProperties":false,"properties":{"n":{"type":"number","minimum":0.25,"maximum":4}}}}}},"responses":{"200":{"description":"control"}}}}},"components":{"schemas":{}}});
    for (minimum, maximum, kind) in [
        (0.1, 4.0, "number next_down"),
        (0.25, 4.01, "number next_up"),
    ] {
        let run = run_body_with(
            &doc,
            "POST",
            "/v1/speed",
            json!({"n":1}),
            &[],
            &[],
            &async |body: Value| {
                let accepted = body.as_object().is_some_and(|o| o.len() == 1)
                    && body["n"]
                        .as_f64()
                        .is_some_and(|n| (minimum..=maximum).contains(&n));
                Probe::from(Captured {
                    status: if accepted {
                        StatusCode::OK
                    } else {
                        StatusCode::BAD_REQUEST
                    },
                    headers: HeaderMap::new(),
                    bytes: Bytes::from_static(b"{}"),
                    method: "POST".into(),
                    template: Some("/v1/speed".into()),
                    query: vec![],
                })
            },
        )
        .await;
        let error = match run {
            Ok(_) => panic!("slightly loose number bound escaped vectors"),
            Err(error) => error,
        };
        assert!(
            error.to_string().contains(kind),
            "number-neighbour control failed for another case"
        );
    }
}
