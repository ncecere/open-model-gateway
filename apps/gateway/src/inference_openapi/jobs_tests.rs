//! Job contract regression controls (#82 PR3b).
use super::{responses::validate_value, tests::generated};
use serde_json::json;

#[test]
fn inference_openapi_batch_endpoint_is_closed() {
    let doc = generated();
    let schema = &doc["components"]["schemas"]["post_v1_batchesRequest"];
    for endpoint in [
        "/v1/chat/completions",
        "/v1/responses",
        "/v1/embeddings",
        "/v1/messages",
    ] {
        let mut body = json!({"input_file_id":"file-00000000000000000000000000000000","endpoint":endpoint,"completion_window":"24h"});
        assert!(
            validate_value(&doc, schema, &body).is_ok(),
            "allowed batch endpoint"
        );
        body["endpoint"] = json!("/v1/audio/speech");
        assert!(
            validate_value(&doc, schema, &body).is_err(),
            "unsupported batch endpoint accepted"
        );
    }
}

#[test]
fn inference_openapi_video_create_is_refusal_only() {
    let doc = generated();
    let responses = &doc["paths"]["/v1/videos"]["post"]["responses"];
    assert!(
        responses.get("200").is_none(),
        "video creation promises success"
    );
    assert_eq!(responses["400"]["x-omg-positive-kind"], "refusal");
    assert_eq!(
        doc["paths"]["/v1/videos"]["post"]["x-omg-refusals"][0]["code"],
        "unsupported_capability"
    );
}

#[test]
fn inference_openapi_batch_errors_have_line_reports() {
    let doc = generated();
    let schema = &doc["components"]["schemas"]["BatchInvalidInput"];
    let valid = json!({"error":{"message":"Invalid lines","type":"invalid_request_error","code":"invalid_batch_input","param":"input_file_id"},"errors":{"object":"list","data":[{"code":"invalid_line","message":"Invalid line","line":1,"param":null}]}});
    assert!(
        validate_value(&doc, schema, &valid).is_ok(),
        "line report rejected"
    );
    let mut bad = valid;
    bad["errors"]["data"] = json!({});
    assert!(
        validate_value(&doc, schema, &bad).is_err(),
        "non-array line report accepted"
    );
}

#[cfg(feature = "integration-tests")]
mod fixtures {
    use super::super::{
        fixtures_tests::{gateway, request},
        responses::{self as r, Fixtures, capture, probe},
    };
    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
    use serde_json::{Value, json};
    use sqlx::PgPool;
    use tower::ServiceExt;

    async fn exchange(
        app: &axum::Router,
        method: &str,
        path: &str,
        key: &str,
        body: Option<&Value>,
    ) -> crate::management::openapi::responses::Captured {
        capture(
            method,
            path,
            app.clone()
                .oneshot(request(method, path, Some(key), body))
                .await
                .unwrap(),
        )
        .await
    }
    async fn upload(
        app: &axum::Router,
        key: &str,
        data: &str,
        first: bool,
    ) -> crate::management::openapi::responses::Captured {
        let field = "--B\r\nContent-Disposition: form-data; name=\"purpose\"\r\n\r\nbatch\r\n";
        let file = format!(
            "--B\r\nContent-Disposition: form-data; name=\"file\"; filename=\"input.jsonl\"\r\nContent-Type: application/octet-stream\r\n\r\n{data}\r\n"
        );
        let body = if first {
            format!(
                "{file}{field}--B\r\nContent-Disposition: form-data; name=\"expires_after[anchor]\"\r\n\r\ncreated_at\r\n--B\r\nContent-Disposition: form-data; name=\"expires_after[seconds]\"\r\n\r\n3600\r\n--B--\r\n"
            )
        } else {
            format!("{field}{file}--B--\r\n")
        };
        capture(
            "POST",
            "/v1/files",
            app.clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/v1/files")
                        .header("authorization", format!("Bearer {key}"))
                        .header("content-type", "multipart/form-data; boundary=B")
                        .body(Body::from(body))
                        .unwrap(),
                )
                .await
                .unwrap(),
        )
        .await
    }
    async fn upload_content(
        app: &axum::Router,
        key: &str,
        purpose: &str,
        data: &[u8],
    ) -> crate::management::openapi::responses::Captured {
        let mut body=format!("--B\r\nContent-Disposition: form-data; name=\"purpose\"\r\n\r\n{purpose}\r\n--B\r\nContent-Disposition: form-data; name=\"file\"; filename=\"fixture.bin\"\r\nContent-Type: application/octet-stream\r\n\r\n").into_bytes();
        body.extend_from_slice(data);
        body.extend_from_slice(b"\r\n--B--\r\n");
        capture(
            "POST",
            "/v1/files",
            app.clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/v1/files")
                        .header("authorization", format!("Bearer {key}"))
                        .header("content-type", "multipart/form-data; boundary=B")
                        .body(Body::from(body))
                        .unwrap(),
                )
                .await
                .unwrap(),
        )
        .await
    }
    fn content_refusal(c: &crate::management::openapi::responses::Captured) {
        assert_eq!(
            c.status,
            StatusCode::BAD_REQUEST,
            "file content refusal status"
        );
        let body: Value = serde_json::from_slice(&c.bytes).expect("validated file error JSON kind");
        assert!(
            body["error"]["code"] == "invalid_request_error"
                && body["error"]["type"] == "invalid_request_error"
                && body["error"]["param"] == "file",
            "file content refusal kind or parameter pointer"
        );
    }
    fn id(c: &crate::management::openapi::responses::Captured) -> String {
        serde_json::from_slice::<Value>(&c.bytes).unwrap()["id"]
            .as_str()
            .expect("object id kind")
            .to_owned()
    }

    async fn legacy_batch(pool: &PgPool, key: &str, state: &str) -> String {
        use crate::inference::repository::{ExecutionStart, InferenceRepository};
        let store = crate::store::Store::new(pool.clone());
        let principal = store
            .authenticate(key)
            .await
            .unwrap()
            .expect("fixture principal kind");
        let deployment: uuid::Uuid = sqlx::query_scalar("SELECT id FROM deployments")
            .fetch_one(pool)
            .await
            .unwrap();
        let execution = uuid::Uuid::new_v4();
        store
            .start(&ExecutionStart {
                id: execution,
                root_request_id: execution,
                attempt_number: 1,
                principal,
                deployment_id: deployment,
                provider: "test_vendor".into(),
                model: "company/smart".into(),
                streamed: false,
                upstream_model: None,
                client: Default::default(),
            })
            .await
            .unwrap();
        let id = uuid::Uuid::new_v4();
        let upstream = format!("contract_{state}_{}", uuid::Uuid::new_v4().simple());
        sqlx::query("INSERT INTO async_jobs(id,kind,workspace_id,api_key_id,deployment_id,execution_id,public_model,provider,upstream_id,batch_endpoint,poll_deadline_at) VALUES($1,'batch',$2,$3,$4,$5,'company/smart','test_vendor',$6,'/v1/embeddings',now()+interval '1 hour')").bind(id).bind(principal.workspace_id).bind(principal.key_id).bind(deployment).bind(execution).bind(upstream).execute(pool).await.unwrap();
        // Historical provider-file records have no encrypted-store input and
        // no known counts. They remain readable/cancellable without fabricating
        // either, and are distinct from current stored-file-backed batches.
        for purpose in ["batch_output", "batch_error"] {
            sqlx::query("INSERT INTO async_job_files(id,workspace_id,api_key_id,deployment_id,public_model,upstream_id,purpose,job_id) VALUES($1,$2,$3,$4,'company/smart',$5,$6,$7)").bind(uuid::Uuid::new_v4()).bind(principal.workspace_id).bind(principal.key_id).bind(deployment).bind(format!("file_{}",uuid::Uuid::new_v4().simple())).bind(purpose).bind(id).execute(pool).await.unwrap();
        }
        format!("batch_{}", id.simple())
    }

    #[sqlx::test(migrations = "./enterprise_migrations")]
    async fn inference_openapi_jobs_wire_fixtures(pool: PgPool) {
        let (app, keys) = gateway(&pool).await;
        sqlx::query(
            "UPDATE installation_settings SET file_batch_enabled=true,file_user_files_enabled=true",
        )
        .execute(&pool)
        .await
        .unwrap();
        use crate::billing::v3::Meter;
        let lines:Vec<_>=Meter::ALL.into_iter().map(|meter| {
            if meter.is_token() || meter==Meter::Requests {
                let batch=if meter.is_token(){1000000}else{1};
                json!({"meter":meter.as_str(),"microusd_per_batch":"0","batch":batch,"unit_label":meter.unit_label(batch).unwrap(),"sku_label":"fixture"})
            } else {json!({"meter":meter.as_str(),"not_applicable":true})}
        }).collect();
        let deployment: uuid::Uuid = sqlx::query_scalar("SELECT id FROM deployments")
            .fetch_one(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO deployment_prices(id,deployment_id,input_token_limit,output_token_limit,pricing_version,price_lines,max_units) VALUES($1,$2,1000,1000,3,$3,'{}')").bind(uuid::Uuid::new_v4()).bind(deployment).bind(json!(lines)).execute(&pool).await.unwrap();
        let app = app.layer(axum::Extension(crate::filestore::FileStoreRuntime::memory()));
        let key = &keys.personal_key.token;
        let mut fixtures = Fixtures::default();
        let line=json!({"custom_id":"one","method":"POST","url":"/v1/embeddings","body":{"model":"company/smart","input":"hello"}}).to_string()+"\n";
        let file = upload(&app, key, &line, false).await;
        assert_eq!(file.status, StatusCode::OK, "file upload status");
        fixtures.observe(&file);
        let file_id = id(&file);
        // A known purpose switches receive() to into_last_part: unlike the
        // file-before-purpose control below, no trailing expiry is accepted.
        let invalid_order = format!(
            "--B\r\nContent-Disposition: form-data; name=\"purpose\"\r\n\r\nbatch\r\n--B\r\nContent-Disposition: form-data; name=\"file\"; filename=\"input.jsonl\"\r\n\r\n{line}\r\n--B\r\nContent-Disposition: form-data; name=\"expires_after[anchor]\"\r\n\r\ncreated_at\r\n--B\r\nContent-Disposition: form-data; name=\"expires_after[seconds]\"\r\n\r\n3600\r\n--B--\r\n"
        );
        let c = capture(
            "POST",
            "/v1/files",
            app.clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/v1/files")
                        .header("authorization", format!("Bearer {key}"))
                        .header("content-type", "multipart/form-data; boundary=B")
                        .body(Body::from(invalid_order))
                        .unwrap(),
                )
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(
            c.status,
            StatusCode::BAD_REQUEST,
            "purpose-first trailing expiry status"
        );
        let error: Value =
            serde_json::from_slice(&c.bytes).expect("validated ordering refusal JSON kind");
        assert!(
            error["error"]["code"] == "invalid_request_error"
                && error["error"]["param"].is_null()
                && error["error"]["message"] == "Malformed multipart body",
            "purpose-first trailing expiry refusal kind"
        );
        fixtures.observe(&c);

        use base64::Engine as _;
        let png = base64::engine::general_purpose::STANDARD
            .decode(crate::inference::images::fixtures::PNG_1X1)
            .expect("PNG fixture encoding kind");
        for (purpose, data) in [
            ("user_data", &b"opaque fixture"[..]),
            ("vision", png.as_slice()),
        ] {
            let c = upload_content(&app, key, purpose, data).await;
            assert_eq!(c.status, StatusCode::OK, "purpose-specific upload status");
            let body: Value = serde_json::from_slice(&c.bytes).expect("validated file JSON kind");
            assert!(body["purpose"] == purpose, "uploaded purpose kind");
            fixtures.observe(&c);
            let downloaded = exchange(
                &app,
                "GET",
                &format!("/v1/files/{}/content", id(&c)),
                key,
                None,
            )
            .await;
            assert_eq!(
                downloaded.status,
                StatusCode::OK,
                "purpose-specific download status"
            );
            assert!(
                downloaded.bytes.as_ref() == data,
                "purpose-specific binary round trip"
            );
            fixtures.observe(&downloaded);
        }
        for (purpose, data) in [
            ("user_data", &b""[..]),
            ("batch", &b"[]\n"[..]),
            ("batch", &b"{\0}\n"[..]),
            ("vision", &b"not an image"[..]),
        ] {
            let c = upload_content(&app, key, purpose, data).await;
            content_refusal(&c);
            fixtures.observe(&c);
        }
        let bad_cursor = exchange(&app, "GET", "/v1/files?after=invalid", key, None).await;
        assert_eq!(
            bad_cursor.status,
            StatusCode::BAD_REQUEST,
            "file-list malformed cursor status"
        );

        let second = upload(&app, key, &line, true).await;
        assert_eq!(second.status, StatusCode::OK, "file-first status");
        fixtures.observe(&second);
        for path in [
            format!("/v1/files/{file_id}"),
            format!("/v1/files/{file_id}/content"),
            format!("/v1/files/{}", id(&second)),
        ] {
            let c = exchange(&app, "GET", &path, key, None).await;
            assert_eq!(c.status, StatusCode::OK, "file read status");
            fixtures.observe(&c);
        }
        fixtures
            .query_vectors(
                "GET",
                "/v1/files",
                &[
                    ("purpose", "batch"),
                    ("limit", "1"),
                    ("order", "asc"),
                    ("after", &file_id),
                ],
                async || {
                    (
                        (),
                        vec![
                            ("purpose".into(), "batch".into()),
                            ("limit".into(), "2".into()),
                            ("order".into(), "asc".into()),
                            ("after".into(), file_id.clone()),
                        ],
                    )
                },
                async |_, query| {
                    probe(exchange(&app, "GET", &format!("/v1/files{query}"), key, None).await)
                },
            )
            .await;
        let body =
            json!({"input_file_id":file_id,"endpoint":"/v1/embeddings","completion_window":"24h"});
        let batch = exchange(&app, "POST", "/v1/batches", key, Some(&body)).await;
        assert_eq!(batch.status, StatusCode::OK, "batch create status");
        fixtures.observe(&batch);
        let batch_id = id(&batch);
        let bad_metadata=exchange(&app,"POST","/v1/batches",key,Some(&json!({"input_file_id":file_id,"endpoint":"/v1/embeddings","completion_window":"24h","metadata":{"omg_retries":"3"}}))).await;
        assert_eq!(
            bad_metadata.status,
            StatusCode::BAD_REQUEST,
            "invalid batch metadata status"
        );
        let body: Value =
            serde_json::from_slice(&bad_metadata.bytes).expect("validated batch error JSON kind");
        assert!(
            body["error"]["code"] == "invalid_metadata",
            "batch metadata refusal kind"
        );
        sqlx::query("UPDATE installation_settings SET file_batch_enabled=false")
            .execute(&pool)
            .await
            .unwrap();
        let disabled=exchange(&app,"POST","/v1/batches",key,Some(&json!({"input_file_id":file_id,"endpoint":"/v1/embeddings","completion_window":"24h"}))).await;
        sqlx::query("UPDATE installation_settings SET file_batch_enabled=true")
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(
            disabled.status,
            StatusCode::BAD_REQUEST,
            "disabled batch files status"
        );
        let body: Value =
            serde_json::from_slice(&disabled.bytes).expect("validated batch error JSON kind");
        assert!(
            body["error"]["code"] == "batch_files_disabled",
            "batch files refusal kind"
        );

        let initial_list = exchange(&app, "GET", "/v1/batches", key, None).await;
        assert_eq!(
            initial_list.status,
            StatusCode::OK,
            "uncancelled batch list status"
        );
        fixtures.observe(&initial_list);
        for (method, path) in [
            ("GET", format!("/v1/batches/{batch_id}")),
            ("POST", format!("/v1/batches/{batch_id}/cancel")),
        ] {
            let c = exchange(&app, method, &path, key, None).await;
            assert_eq!(c.status, StatusCode::OK, "batch read/cancel status");
            fixtures.observe(&c);
        }
        fixtures
            .query_vectors(
                "GET",
                "/v1/batches",
                &[("limit", "1"), ("order", "desc"), ("after", &batch_id)],
                async || {
                    (
                        (),
                        vec![
                            ("limit".into(), "20".into()),
                            ("order".into(), "desc".into()),
                            ("after".into(), batch_id.clone()),
                        ],
                    )
                },
                async |_, query| {
                    probe(exchange(&app, "GET", &format!("/v1/batches{query}"), key, None).await)
                },
            )
            .await;
        for state in ["completed", "failed", "expired", "cancelled"] {
            let legacy = legacy_batch(&pool, key, state).await;
            let before = exchange(&app, "GET", &format!("/v1/batches/{legacy}"), key, None).await;
            assert_eq!(before.status, StatusCode::OK, "legacy batch read status");
            fixtures.observe(&before);
            let c = exchange(
                &app,
                "POST",
                &format!("/v1/batches/{legacy}/cancel"),
                key,
                None,
            )
            .await;
            assert_eq!(c.status, StatusCode::OK, "poller-race cancel status");
            fixtures.observe(&c);
            let after = exchange(&app, "GET", &format!("/v1/batches/{legacy}"), key, None).await;
            assert_eq!(after.status, StatusCode::OK, "terminal batch read status");
            fixtures.observe(&after);
        }
        let all = exchange(&app, "GET", "/v1/batches", key, None).await;
        assert_eq!(all.status, StatusCode::OK, "batch lifecycle list status");
        fixtures.observe(&all);
        let invalid = upload(&app, key, "{}\n", false).await;
        assert_eq!(
            invalid.status,
            StatusCode::OK,
            "invalid lines upload status"
        );
        let c=exchange(&app,"POST","/v1/batches",key,Some(&json!({"input_file_id":id(&invalid),"endpoint":"/v1/embeddings","completion_window":"24h"}))).await;
        assert_eq!(
            c.status,
            StatusCode::BAD_REQUEST,
            "batch line report status"
        );
        let report: Value = serde_json::from_slice(&c.bytes).unwrap();
        assert!(report["errors"]["data"].is_array(), "line report kind");
        // JSON boundary probes use fresh valid files, so claiming one input
        // never changes the input verdict of another vector.
        for valid in [
            json!({"input_file_id":file_id,"endpoint":"/v1/embeddings","completion_window":"24h","metadata":null,"output_expires_after":null}),
            json!({"input_file_id":file_id,"endpoint":"/v1/embeddings","completion_window":"24h","metadata":{"tag":"fixture","omg_mode":"gateway","omg_retries":"0"},"output_expires_after":null}),
            json!([file_id, "/v1/embeddings", "24h", null, null]),
        ] {
            fixtures
                .body_vectors("POST", "/v1/batches", valid, &[], &[], async |mut body| {
                    if body
                        .get("input_file_id")
                        .or_else(|| body.get(0))
                        .is_some_and(|value| value.as_str() == Some(file_id.as_str()))
                    {
                        let f = upload(&app, key, &line, false).await;
                        assert_eq!(f.status, StatusCode::OK, "vector file status");
                        if body.is_array() {
                            body[0] = json!(id(&f));
                        } else {
                            body["input_file_id"] = json!(id(&f));
                        }
                    }
                    let c = exchange(&app, "POST", "/v1/batches", key, Some(&body)).await;
                    let parsed: Value =
                        serde_json::from_slice(&c.bytes).expect("batch vector JSON kind");
                    if c.status == StatusCode::OK {
                        let created = parsed["id"].as_str().expect("batch id kind");
                        let cancelled = exchange(
                            &app,
                            "POST",
                            &format!("/v1/batches/{created}/cancel"),
                            key,
                            None,
                        )
                        .await;
                        assert_eq!(
                            cancelled.status,
                            StatusCode::OK,
                            "vector batch cancel status"
                        );
                    }
                    if c.status == StatusCode::BAD_REQUEST
                        && parsed["error"]["code"] == "invalid_batch_input"
                    {
                        // File-plan semantics (endpoint versus stored lines), not
                        // rejection of the accepted JSON DTO's field vocabulary.
                        crate::management::openapi::http_vectors::Probe::decided(c, true)
                    } else {
                        probe(c)
                    }
                })
                .await;
        }
        let c = exchange(
            &app,
            "DELETE",
            &format!("/v1/files/{}", id(&second)),
            key,
            None,
        )
        .await;
        assert_eq!(c.status, StatusCode::OK, "file delete status");
        fixtures.observe(&c);
        // No production adapter offers video creation; valid requests must
        // remain explicit refusals, while legacy reads are tested separately.
        let invalid_video = exchange(
            &app,
            "POST",
            "/v1/videos",
            key,
            Some(&json!({"unexpected":true})),
        )
        .await;
        assert_eq!(
            invalid_video.status,
            StatusCode::BAD_REQUEST,
            "video validation status"
        );
        fixtures.observe(&invalid_video);
        assert!(
            !fixtures.has_positive("POST", "/v1/videos"),
            "video validation earned refusal credit"
        );
        let video = exchange(
            &app,
            "POST",
            "/v1/videos",
            key,
            Some(&json!({"model":"company/smart","prompt":"fixture"})),
        )
        .await;
        assert_eq!(
            video.status,
            StatusCode::BAD_REQUEST,
            "video refusal status"
        );
        fixtures.observe(&video);
        fixtures.finish(r::FILE_BATCH_AREAS);
    }
}

#[cfg(feature = "integration-tests")]
mod legacy {
    use super::super::responses::{self as r, Fixtures, capture, probe};
    use crate::{
        governance::tests::db::fixture,
        inference::{Engine, EngineLimits, error::InferenceError, types::*},
        jobs::{
            db_tests::{Fake, bearer, jobs, upstream_video, video_request, video_setup},
            types::*,
        },
        providers::{ProviderAdapter, ProviderRegistry},
    };
    use async_trait::async_trait;
    use axum::{
        body::{Body, Bytes},
        http::{Request, StatusCode},
    };
    use serde_json::json;
    use sqlx::PgPool;
    use std::sync::Arc;
    use tower::ServiceExt;

    struct Legacy(Fake);
    #[async_trait]
    impl ProviderAdapter for Legacy {
        fn id(&self) -> &'static str {
            "openai"
        }
        fn capabilities(&self) -> Capabilities {
            self.0.capabilities()
        }
        fn supports_protocol(&self, p: ApiProtocol) -> bool {
            p == ApiProtocol::Videos
        }
        async fn execute(
            &self,
            _: &Deployment,
            _: ChatRequest,
        ) -> Result<ProviderOutput, InferenceError> {
            Err(InferenceError::Unsupported)
        }
        async fn retrieve_video(
            &self,
            d: &Deployment,
            id: &UpstreamId,
        ) -> Result<UpstreamVideo, InferenceError> {
            self.0.retrieve_video(d, id).await
        }
        async fn delete_video(
            &self,
            d: &Deployment,
            id: &UpstreamId,
        ) -> Result<(), InferenceError> {
            self.0.delete_video(d, id).await
        }
        async fn video_content(
            &self,
            _: &Deployment,
            _: &UpstreamId,
            asset: VideoAsset,
        ) -> Result<ContentStream, InferenceError> {
            Ok(ContentStream {
                content_type: if asset == VideoAsset::Video {
                    "video/mp4"
                } else {
                    "image/webp"
                },
                content_length: Some(3),
                body: Box::pin(futures::stream::iter([Ok(Bytes::from_static(b"vid"))])),
            })
        }
    }
    #[sqlx::test(migrations = "./enterprise_migrations")]
    async fn inference_openapi_legacy_video_wire_fixtures(pool: PgPool) {
        let f = fixture(pool).await;
        video_setup(&f).await;
        let fake = Fake::default();
        let jobs = jobs(&f, &fake, false);
        // Create historical rows through the existing test-only provider, not
        // through the refusal-only public create operation.
        fake.push_video(upstream_video(JobState::Completed, Some(8)));
        let job = jobs
            .create_video(f.principal, video_request(), uuid::Uuid::new_v4())
            .await
            .unwrap();
        let id = client_id("video_", job.id);
        fake.push_video(upstream_video(JobState::Queued, Some(8)));
        let queued = jobs
            .create_video(f.principal, video_request(), uuid::Uuid::new_v4())
            .await
            .unwrap();
        let queued_id = client_id("video_", queued.id);
        let mut failed_observation = upstream_video(JobState::Failed, Some(8));
        failed_observation.error = Some(ErrorCode::parse("moderation_blocked"));
        failed_observation.expires_at =
            Some((chrono::Utc::now() + chrono::TimeDelta::hours(1)).timestamp());
        fake.push_video(failed_observation.clone());
        let failed = jobs
            .create_video(f.principal, video_request(), uuid::Uuid::new_v4())
            .await
            .unwrap();
        let failed_id = client_id("video_", failed.id);
        let mut registry = ProviderRegistry::default();
        registry.register(Arc::new(Legacy(fake.clone()))).unwrap();
        let engine =
            Engine::new(Arc::new(f.store.clone()), registry, EngineLimits::default()).unwrap();
        let app = crate::http::router_with_engine(f.store.clone(), None, engine);
        let key = bearer(&f, f.principal.workspace_id).await;
        let send = async |method: &str, path: String| {
            capture(
                method,
                &path,
                app.clone()
                    .oneshot(
                        Request::builder()
                            .method(method)
                            .uri(&path)
                            .header("authorization", &key)
                            .body(Body::empty())
                            .unwrap(),
                    )
                    .await
                    .unwrap(),
            )
            .await
        };
        let mut fixtures = Fixtures::default();
        let list = send("GET", "/v1/videos".into()).await;
        assert_eq!(list.status, StatusCode::OK, "legacy list status");
        fixtures.observe(&list);
        let c = send("GET", format!("/v1/videos/{id}")).await;
        assert_eq!(c.status, StatusCode::OK, "legacy retrieve status");
        fixtures.observe(&c);
        fake.push_video(upstream_video(JobState::Queued, Some(8)));
        let c = send("GET", format!("/v1/videos/{queued_id}")).await;
        assert_eq!(c.status, StatusCode::OK, "queued legacy read status");
        fixtures.observe(&c);
        fake.push_video(failed_observation);
        let c = send("GET", format!("/v1/videos/{failed_id}")).await;
        assert_eq!(c.status, StatusCode::OK, "failed legacy read status");
        fixtures.observe(&c);
        for variant in ["video", "thumbnail", "spritesheet"] {
            let c = send("GET", format!("/v1/videos/{id}/content?variant={variant}")).await;
            assert_eq!(c.status, StatusCode::OK, "legacy content status");
            fixtures.observe(&c);
        }
        fixtures
            .query_vectors(
                "GET",
                "/v1/videos/{id}/content",
                &[("variant", "video")],
                async || ((), vec![]),
                async |_, query| {
                    probe(send("GET", format!("/v1/videos/{id}/content{query}")).await)
                },
            )
            .await;
        fixtures
            .query_vectors(
                "GET",
                "/v1/videos",
                &[("limit", "1"), ("order", "asc"), ("after", &id)],
                async || {
                    (
                        (),
                        vec![
                            ("limit".into(), "20".into()),
                            ("order".into(), "desc".into()),
                            ("after".into(), id.clone()),
                        ],
                    )
                },
                async |_, query| probe(send("GET", format!("/v1/videos{query}")).await),
            )
            .await;
        let c = send("DELETE", format!("/v1/videos/{id}")).await;
        assert_eq!(c.status, StatusCode::OK, "legacy delete status");
        fixtures.observe(&c);
        // Public create is exercised with the actual registry, which has no
        // video provider, rather than the historical read fixture above.
        let refusal = crate::http::router(f.store.clone())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/videos")
                    .header("authorization", &key)
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({"model":"company/smart","prompt":"fixture"}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        let c = capture("POST", "/v1/videos", refusal).await;
        assert_eq!(
            c.status,
            StatusCode::BAD_REQUEST,
            "video create refusal status"
        );
        fixtures.observe(&c);
        let refusal_app = crate::http::router(f.store.clone());
        for seconds in [json!(8), json!("8"), serde_json::Value::Null] {
            let size = if seconds.is_null() {
                serde_json::Value::Null
            } else {
                json!("720x1280")
            };
            for valid in [
                json!({"model":"company/smart","prompt":"fixture","seconds":seconds,"size":size,"input_reference":null}),
                json!(["company/smart", "fixture", seconds, size, null]),
            ] {
                fixtures
                    .body_vectors("POST", "/v1/videos", valid, &[], &[], async |body| {
                        let reference = body.get("input_reference").or_else(|| body.get(4));
                        let unsupported_reference = reference.is_some_and(|r| !r.is_null());
                        let response = refusal_app
                            .clone()
                            .oneshot(
                                Request::builder()
                                    .method("POST")
                                    .uri("/v1/videos")
                                    .header("authorization", &key)
                                    .header("content-type", "application/json")
                                    .body(Body::from(body.to_string()))
                                    .unwrap(),
                            )
                            .await
                            .unwrap();
                        let c = capture("POST", "/v1/videos", response).await;
                        let parsed: serde_json::Value =
                            serde_json::from_slice(&c.bytes).expect("video vector JSON kind");
                        let accepted = c.status == StatusCode::BAD_REQUEST
                            && parsed["error"]["code"] == "unsupported_capability"
                            && !unsupported_reference;
                        crate::management::openapi::http_vectors::Probe::decided(c, accepted)
                    })
                    .await;
            }
        }
        let form = "--B\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\ncompany/smart\r\n--B\r\nContent-Disposition: form-data; name=\"prompt\"\r\n\r\nfixture\r\n--B\r\nContent-Disposition: form-data; name=\"seconds\"\r\n\r\n8\r\n--B\r\nContent-Disposition: form-data; name=\"size\"\r\n\r\n720x1280\r\n--B--\r\n";
        let response = refusal_app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/videos")
                    .header("authorization", &key)
                    .header("content-type", "multipart/form-data; boundary=B")
                    .body(Body::from(form))
                    .unwrap(),
            )
            .await
            .unwrap();
        let c = capture("POST", "/v1/videos", response).await;
        assert_eq!(
            c.status,
            StatusCode::BAD_REQUEST,
            "multipart video refusal status"
        );
        let body: serde_json::Value =
            serde_json::from_slice(&c.bytes).expect("validated video error JSON kind");
        assert!(
            body["error"]["code"] == "unsupported_capability",
            "multipart video refusal kind"
        );
        fixtures.observe(&c);
        fixtures.finish(r::VIDEO_AREAS);
    }
}

#[tokio::test]
async fn inference_openapi_files_auth_storage_failure_uses_transport_envelope() {
    use axum::{body::Body, http::Request};
    use tower::ServiceExt;
    let pool = sqlx::postgres::PgPoolOptions::new()
        .acquire_timeout(std::time::Duration::from_millis(20))
        .connect_lazy("postgres://unused:unused@127.0.0.1:1/unused")
        .unwrap();
    let app = crate::http::router(crate::store::Store::new(pool));
    let token = format!("omg_{}.{}", "0".repeat(32), "0".repeat(64));
    let response = app
        .oneshot(
            Request::builder()
                .uri("/v1/files")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let (parts, body) = response.into_parts();
    let bytes = axum::body::to_bytes(body, 65536).await.unwrap();
    assert_eq!(
        parts.status.as_u16(),
        503,
        "Files authentication storage status"
    );
    super::responses::validate_documented(
        &super::tests::generated(),
        "GET",
        "/v1/files",
        parts.status,
        &parts.headers,
        &bytes,
    )
    .unwrap();
}

#[test]
fn inference_openapi_followup_file_part_rules_are_explicit() {
    let doc = generated();
    let schema = &doc["components"]["schemas"]["post_v1_filesRequest"];
    assert!(
        validate_value(&doc, schema, &json!({"purpose":"user_data","file":""})).is_err(),
        "empty file accepted by schema"
    );
    let rules = &doc["paths"]["/v1/files"]["post"]["x-omg-multipart"]["file_content_rules"];
    assert_eq!(rules["sniff_bytes"], 4096, "file sniff bound missing");
    assert_eq!(
        rules["batch"]["first_non_whitespace_byte"], "{",
        "batch head rule missing"
    );
    assert_eq!(
        rules["batch"]["reject_nul_in_head"], true,
        "batch NUL rule missing"
    );
    assert_eq!(
        rules["vision"]["signatures"],
        json!(["PNG", "JPEG", "GIF87a", "GIF89a", "RIFF/WEBP"]),
        "vision signature rules missing"
    );
}

#[test]
fn inference_openapi_followup_refusals_match_handler_kinds() {
    let doc = generated();
    for (path, method, status, code, envelope) in [
        (
            "/v1/files",
            "get",
            400,
            "invalid_request_error",
            "FileErrorEnvelope",
        ),
        (
            "/v1/files",
            "post",
            400,
            "invalid_request_error",
            "FileErrorEnvelope",
        ),
        (
            "/v1/batches",
            "post",
            400,
            "invalid_metadata",
            "JobErrorEnvelope",
        ),
        (
            "/v1/batches",
            "post",
            400,
            "batch_files_disabled",
            "JobErrorEnvelope",
        ),
        (
            "/v1/batches/{id}/cancel",
            "post",
            403,
            "batch_not_owned",
            "JobErrorEnvelope",
        ),
        (
            "/v1/batches/{id}/cancel",
            "post",
            400,
            "batch_not_cancellable",
            "JobErrorEnvelope",
        ),
    ] {
        let rows = doc["paths"][path][method]["x-omg-refusals"]
            .as_array()
            .expect("refusal table kind");
        assert!(
            rows.iter()
                .any(|r| r["status"] == status && r["code"] == code && r["envelope"] == envelope),
            "handler refusal row missing for {method} {path}"
        );
    }
    assert!(
        !doc["paths"]["/v1/files"]["get"]["x-omg-refusals"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["status"] == 404),
        "file-list cursor incorrectly documented as not found"
    );
}

#[test]
fn inference_openapi_followup_video_multipart_matches_json_bounds() {
    let doc = generated();
    let schema = &doc["paths"]["/v1/videos"]["post"]["requestBody"]["content"]["multipart/form-data"]
        ["schema"];
    let fields = &doc["components"]["schemas"]["post_v1_videosRequest"]["oneOf"][0]["properties"];
    for field in ["model", "prompt"] {
        for keyword in ["type", "pattern", "maxLength", "x-omg-max-bytes"] {
            assert!(
                schema["properties"][field][keyword] == fields[field][keyword],
                "multipart JSON constraint drift: /properties/{field}/{keyword}"
            );
        }
    }
    assert!(
        schema["properties"]["seconds"] == fields["seconds"]["anyOf"][0]["oneOf"][1],
        "multipart seconds enum drift"
    );
    assert!(
        schema["properties"]["size"] == fields["size"]["anyOf"][0],
        "multipart size enum drift"
    );
    let valid = json!({"model":"m","prompt":"p","seconds":"4","size":"720x1280"});
    assert!(
        validate_value(&doc, schema, &valid).is_ok(),
        "valid video multipart fields rejected"
    );
    for (field, value) in [
        ("seconds", json!("5")),
        ("size", json!("auto")),
        ("model", json!("m".repeat(201))),
        ("prompt", json!("p".repeat(32769))),
        ("model", json!(" ")),
        ("prompt", json!(" ")),
    ] {
        let mut bad = valid.clone();
        bad[field] = value;
        assert!(
            validate_value(&doc, schema, &bad).is_err(),
            "invalid video multipart field accepted: {field}"
        );
    }
}

const FILE_UPLOAD_ORDERING: &str = "When purpose precedes file, file must be the last part and all expiry fields must precede it. When file precedes purpose, expiry fields may precede or follow the file; trailing purpose/expiry fields are read before the provisional upload is committed, moved or discarded.";

fn exact_file_ordering(doc: &serde_json::Value) -> bool {
    doc["paths"]["/v1/files"]["post"]["x-omg-multipart"]["ordering"].as_str()
        == Some(FILE_UPLOAD_ORDERING)
}

#[test]
fn inference_openapi_review_file_upload_ordering_is_conditional() {
    let doc = generated();
    assert!(
        exact_file_ordering(&doc),
        "file upload ordering must distinguish purpose-first and file-first"
    );
    let mut bad = doc;
    bad["paths"]["/v1/files"]["post"]["x-omg-multipart"]["ordering"] =
        json!("purpose and expiry before or after the file");
    assert!(
        !exact_file_ordering(&bad),
        "unconditional trailing-expiry wording escaped the pin"
    );
}
