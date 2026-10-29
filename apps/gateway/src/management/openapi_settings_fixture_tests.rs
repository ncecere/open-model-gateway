//! Slice 5: executed settings/alerts contract coverage over a running gateway.
//! No provider calls, secret resolution or webhook worker; test deliveries use
//! saved configuration and local SMTP/audit receivers only.
use super::*;
use openapi::responses::{Captured, PositiveFixtures, capture_http};
use sha2::{Digest, Sha256};

#[path = "openapi_alerts_states_tests.rs"]
mod alerts_states;
#[path = "openapi_settings_states_tests.rs"]
mod states;

const SETTINGS: &str = "/api/v1/platform/settings";
const ALERTS: &str = "/api/v1/platform/alerts";
const ORIGIN: &str = "http://127.0.0.1:3000";
struct Server(tokio::task::JoinHandle<()>);
impl Drop for Server {
    fn drop(&mut self) {
        self.0.abort();
    }
}
struct Browser {
    cookie: String,
    csrf: String,
}
async fn browser(pool: &PgPool, user: &BrowserPrincipal) -> Browser {
    let cookie = crate::entropy::hex_token().unwrap();
    let csrf = crate::entropy::hex_token().unwrap();
    sqlx::query("INSERT INTO browser_sessions(token_hash,user_id,csrf_hash,expires_at,verified_email,last_seen_at) VALUES($1,$2,$3,now()+interval '1 hour',$4,now())")
        .bind(Sha256::digest(cookie.as_bytes()).as_slice()).bind(user.user_id)
        .bind(Sha256::digest(csrf.as_bytes()).as_slice()).bind(&user.email).execute(pool).await.unwrap();
    Browser { cookie, csrf }
}
struct Gateway {
    client: reqwest::Client,
    base: String,
    _server: Server,
}
/// Which sign-in configuration the serving stack runs with.
#[derive(Clone, Copy)]
enum Sign {
    Oidc,
    OidcScim,
    NoProvider,
}
impl Gateway {
    async fn start(s: Store, pool: &PgPool, rt: crate::filestore::FileStoreRuntime) -> Self {
        Self::start_with(s, pool, rt, Sign::Oidc).await
    }
    async fn start_with(
        s: Store,
        pool: &PgPool,
        rt: crate::filestore::FileStoreRuntime,
        sign: Sign,
    ) -> Self {
        let mock = crate::identity::tests::MockProvider::start(None).await;
        let identity = match sign {
            Sign::Oidc => mock.state(pool.clone()).await,
            Sign::OidcScim => mock
                .state(pool.clone())
                .await
                .with_scim(
                    crate::scim::ScimConfig::from_lookup(|name| match name {
                        "GATEWAY_SCIM_TOKEN_ENV" => Some("CONTRACT_SCIM_TOKEN".to_owned()),
                        "CONTRACT_SCIM_TOKEN" => {
                            Some("scim-contract-token-0123456789abcdefghijklmnop".to_owned())
                        }
                        _ => None,
                    })
                    .unwrap(),
                )
                .unwrap(),
            Sign::NoProvider => crate::identity::IdentityState::new(Store::new(pool.clone()), None)
                .await
                .unwrap(),
        };
        let engine = crate::inference::Engine::new(
            std::sync::Arc::new(s.clone()),
            crate::providers::ProviderRegistry::default(),
            crate::inference::EngineLimits::default(),
        )
        .unwrap();
        let app = crate::http::router_with_identity(s, None, engine, identity).layer(Extension(rt));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = Server(tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap()
        }));
        Self {
            client: reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap(),
            base,
            _server: server,
        }
    }
    fn request(
        &self,
        actor: Option<&Browser>,
        method: &str,
        path: &str,
    ) -> reqwest::RequestBuilder {
        let mut request = self
            .client
            .request(method.parse().unwrap(), format!("{}{path}", self.base))
            .header("origin", ORIGIN);
        if let Some(actor) = actor {
            request = request
                .header("cookie", format!("omg_session={}", actor.cookie))
                .header("x-csrf-token", &actor.csrf);
        }
        request
    }
    async fn raw(
        &self,
        actor: Option<&Browser>,
        method: &str,
        path: &str,
        media: &str,
        bytes: Vec<u8>,
    ) -> Captured {
        capture_http(
            method,
            path,
            self.request(actor, method, path)
                .header("content-type", media)
                .body(bytes)
                .send()
                .await
                .unwrap(),
        )
        .await
    }
    async fn send(&self, actor: &Browser, method: &str, path: &str, input: Value) -> Captured {
        self.raw(
            Some(actor),
            method,
            path,
            "application/json",
            serde_json::to_vec(&input).unwrap(),
        )
        .await
    }
}
fn value(r: &Captured) -> Value {
    serde_json::from_slice(&r.bytes).unwrap()
}
fn positive(observed: &mut PositiveFixtures, r: &Captured, method: &str, path: &str) -> Value {
    assert!(
        r.status.is_success() || r.status == StatusCode::GONE,
        "{method} {path}: {}: {}",
        r.status,
        String::from_utf8_lossy(&r.bytes)
    );
    let body = value(r);
    // Every executed representation has independent anti-permissiveness controls.
    for bad in [json!({}), json!(false), {
        let mut leak = body.clone();
        leak["plaintext_inference_token"] = json!("must-not-leak");
        leak
    }] {
        assert!(
            openapi::responses::validate(
                openapi::responses::artifact(),
                method,
                path,
                r.status,
                &r.headers,
                &serde_json::to_vec(&bad).unwrap()
            )
            .is_err(),
            "accepted response mutation: {method} {path}: {bad}"
        );
    }
    observed.observe(r);
    body
}
async fn good(
    g: &Gateway,
    observed: &mut PositiveFixtures,
    actor: &Browser,
    method: &str,
    path: &str,
    template: &str,
    input: Value,
) -> Value {
    let r = g.send(actor, method, path, input).await;
    positive(observed, &r, method, template)
}

async fn reject_json_configuration(
    g: &Gateway,
    actor: &Browser,
    method: &str,
    path: &str,
    valid: Value,
) {
    let mut unknown = valid.clone();
    unknown["unexpected_configuration"] = json!({"nested":true});
    for bad in [unknown, Value::Null, json!(false)] {
        let r = g.send(actor, method, path, bad).await;
        assert_eq!(
            r.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{method} {path}"
        );
        assert!(
            r.headers["content-type"]
                .to_str()
                .unwrap()
                .starts_with("text/plain")
        );
    }
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn management_openapi_slice5_fixtures(pool: PgPool) {
    let mut observed = PositiveFixtures::default();
    let mut f = fixture(&pool).await;
    f.s = f.s.with_alert_webhooks(
        crate::config::AlertWebhookPolicy::parse(
            &["https://receiver.example.test/ingest".into()],
            vec!["HOOK_CURRENT".into(), "HOOK_PREVIOUS".into()],
            crate::config::Environment::Production,
        )
        .unwrap(),
    );
    f.s = f.s.with_secret_allowlists(
        Default::default(),
        crate::providers::secrets::EnvSecrets::from_lookup(
            ["CONTRACT_SMTP_PASSWORD".to_owned()],
            |_| None,
        ),
    );
    let admin = browser(&pool, &f.admin).await;
    let auditor = browser(&pool, &f.auditor).await;
    let owner = browser(&pool, &f.owner).await;
    let (url, seen) = audit_export_tests::receiver().await;
    let exporter = crate::audit_export::Exporter::new(&url, vec![7; 32], true)
        .unwrap()
        .unwrap();
    let setting = crate::audit_export::Setting::On(
        crate::audit_export::Exporter::new(&url, vec![7; 32], true)
            .unwrap()
            .unwrap(),
    );
    crate::audit_export::sync_state(&f.s, &setting)
        .await
        .unwrap();
    f.s = f.s.clone().with_audit_export(std::sync::Arc::new(exporter));
    let g = Gateway::start(
        f.s.clone(),
        &pool,
        crate::filestore::FileStoreRuntime::memory(),
    )
    .await;
    let off = Gateway::start(
        f.s.clone(),
        &pool,
        crate::filestore::FileStoreRuntime::off(),
    )
    .await;

    // The real serving stack denies missing/inference credentials and cross-site
    // mutations, in addition to the unchanged route authorization matrices.
    for (path, item) in openapi::responses::artifact()["paths"].as_object().unwrap() {
        if !openapi::responses::SLICE5_AREAS.contains(&openapi::responses::area(path)) {
            continue;
        }
        let concrete = path
            .replace("{ws}", &f.team.to_string())
            .replace("{id}", &Uuid::nil().to_string());
        for method in item.as_object().unwrap().keys() {
            let method = method.to_ascii_uppercase();
            let r = g
                .raw(None, &method, &concrete, "application/json", b"{}".to_vec())
                .await;
            assert_eq!(r.status, StatusCode::UNAUTHORIZED, "{method} {path}");
        }
    }
    let (_, key) = key(&f, &f.owner, f.team, Value::Null).await;
    let path = format!("{SETTINGS}/general");
    let r = capture_http(
        "GET",
        &path,
        g.client
            .get(format!("{}{path}", g.base))
            .bearer_auth(key["token"].as_str().unwrap())
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    for bad in [
        g.client
            .put(format!("{}{path}", g.base))
            .header("cookie", format!("omg_session={}", admin.cookie))
            .header("origin", ORIGIN),
        g.request(Some(&admin), "PUT", &path)
            .header("origin", "https://foreign.example.test"),
    ] {
        let r = capture_http("PUT", &path, bad.json(&json!({})).send().await.unwrap()).await;
        assert_eq!(r.status, StatusCode::FORBIDDEN);
    }

    for part in [
        "general",
        "privacy",
        "email",
        "storage",
        "sign-in",
        "audit-export",
    ] {
        let path = format!("{SETTINGS}/{part}");
        let b = good(
            &g,
            &mut observed,
            &auditor,
            "GET",
            &path,
            &path,
            Value::Null,
        )
        .await;
        assert!(!b.to_string().contains("client_secret"));
        assert!(
            !b.to_string().contains("/ingest"),
            "audit export never reveals destination path"
        );
    }
    let path = format!("{SETTINGS}/general");
    good(&g,&mut observed,&admin,"PUT",&path,&path,json!({"display_name":"Contract Gateway","support_url":"https://support.example.test","logo_url":null,"human_key_max_lifetime_days":30})).await;
    reject_json_configuration(
        &g,
        &admin,
        "PUT",
        &path,
        json!({"display_name":"Contract Gateway","human_key_max_lifetime_days":30}),
    )
    .await;
    let path = format!("{SETTINGS}/privacy");
    let current = value(&g.send(&admin, "GET", &path, Value::Null).await);
    let policy_override = crate::providers::openrouter::DataCollection::env_override();
    let retention_override = crate::maintenance::retention_from_env().unwrap();
    for (field, expected) in [
        (
            "openrouter_data_collection",
            policy_override.map(|p| json!(p.as_str())),
        ),
        (
            "request_log_retention_days",
            retention_override.map(|d| json!(d)),
        ),
    ] {
        assert_eq!(current[field]["locked"], expected.is_some());
        assert_eq!(
            current[field]["source"],
            if expected.is_some() {
                "environment"
            } else {
                "installation"
            }
        );
        if let Some(expected) = expected {
            assert_eq!(current[field]["value"], expected);
        }
    }
    good(&g,&mut observed,&admin,"PUT",&path,&path,json!({"openrouter_data_collection":current["openrouter_data_collection"]["stored"],"request_log_retention_days":current["request_log_retention_days"]["stored"]})).await;
    reject_json_configuration(
        &g,
        &admin,
        "PUT",
        &path,
        json!({"openrouter_data_collection":"deny"}),
    )
    .await;
    for (field, changed) in [
        ("openrouter_data_collection", json!("allow")),
        ("request_log_retention_days", json!(90)),
    ] {
        if current[field]["locked"] == true {
            let mut input = json!({"openrouter_data_collection":current["openrouter_data_collection"]["stored"],"request_log_retention_days":current["request_log_retention_days"]["stored"]});
            input[field] = changed;
            let r = g.send(&admin, "PUT", &path, input).await;
            assert_eq!(r.status, StatusCode::CONFLICT);
            assert_eq!(
                value(&r)["error"]["reason"],
                "setting_locked_by_environment"
            );
        }
    }
    let (port, mut mail) = crate::email::mock::serve(Default::default()).await;
    let path = format!("{SETTINGS}/email");
    good(&g,&mut observed,&admin,"PUT",&path,&path,json!({"host":"127.0.0.1","port":port,"tls":"none","from_address":"gateway@example.test","from_name":"Contract Gateway"})).await;
    reject_json_configuration(&g, &admin, "PUT", &path, json!({"host":null})).await;
    for part in ["email/test", "storage/test", "audit-export/test"] {
        let path = format!("{SETTINGS}/{part}");
        let body = good(&g, &mut observed, &admin, "POST", &path, &path, json!({})).await;
        assert_eq!(body["ok"], true);
        if part == "email/test" {
            assert!(mail.recv().await.unwrap().rcpt_to[0].contains("admin@example.test"));
        }
        if part == "audit-export/test" {
            assert_eq!(seen.lock().unwrap().len(), 1);
        }
        // No configuration/JSON null/array accepted by Bytes test actions.
        for invalid in [
            json!(null),
            json!([]),
            json!({"url":"https://attacker.example.test"}),
        ] {
            assert_eq!(
                g.send(&admin, "POST", &path, invalid).await.status,
                StatusCode::BAD_REQUEST
            );
        }
        let limit = if part == "storage/test" { 5 } else { 2 };
        for _ in 1..limit {
            assert_eq!(
                g.send(&admin, "POST", &path, json!({})).await.status,
                StatusCode::OK
            );
        }
        let r = g.send(&admin, "POST", &path, json!({})).await;
        assert_eq!(r.status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(value(&r)["error"]["code"], "429");
        assert!(
            !r.headers.contains_key("retry-after"),
            "handler test limiter has retry instruction in message only"
        );
        assert!(
            value(&r)["error"]["message"]
                .as_str()
                .unwrap()
                .contains("try again")
        );
    }
    let path = format!("{SETTINGS}/storage");
    let input = json!({"batch":{"enabled":true,"retention_days":30},"video":{"enabled":false,"retention_days":30},"user_files":{"enabled":false,"retention_days":30},"export":{"retention_days":7}});
    assert_eq!(
        off.send(&admin, "PUT", &path, input.clone()).await.status,
        StatusCode::CONFLICT
    );
    good(&g, &mut observed, &admin, "PUT", &path, &path, input).await;
    let valid = json!({"batch":{"enabled":true,"retention_days":30},"video":{"enabled":false,"retention_days":30},"user_files":{"enabled":false,"retention_days":30},"export":{"retention_days":7}});
    reject_json_configuration(&g, &admin, "PUT", &path, valid.clone()).await;
    for bad in [
        json!({"enabled":true,"retention_days":30,"unexpected_configuration":true}),
        json!({"enabled":null,"retention_days":30}),
        json!({"retention_days":30}),
    ] {
        let mut input = valid.clone();
        input["batch"] = bad;
        assert_eq!(
            g.send(&admin, "PUT", &path, input).await.status,
            StatusCode::UNPROCESSABLE_ENTITY
        );
    }
    let b = value(&off.send(&auditor, "GET", &path, Value::Null).await);
    assert_eq!(b["backend"], "off");
    assert!(
        b["groups"]
            .as_array()
            .unwrap()
            .iter()
            .all(|v| v["active"] == false)
    );
    let path = format!("{SETTINGS}/storage/test");
    assert_eq!(
        off.send(&admin, "POST", &path, json!({})).await.status,
        StatusCode::CONFLICT
    );

    let path = format!("{SETTINGS}/general/logo");
    for (media, bytes) in [
        ("image/png", branding::fixtures::png(64, 64)),
        ("image/jpeg", branding::fixtures::jpeg(64, 64)),
        ("image/webp", branding::fixtures::webp_lossless(64, 64)),
    ] {
        let r = g.raw(Some(&admin), "PUT", &path, media, bytes).await;
        assert!(positive(&mut observed, &r, "PUT", &path)["logo"].is_object());
    }
    assert_eq!(
        g.raw(
            Some(&admin),
            "PUT",
            &path,
            "multipart/form-data; boundary=x",
            b"--x--".to_vec()
        )
        .await
        .status,
        StatusCode::UNSUPPORTED_MEDIA_TYPE
    );
    // Refused uploads keep their documented envelopes: a declared type that is
    // not the bytes, an oversize file and sides outside 16-4096 pixels.
    let mut oversize = branding::fixtures::png(64, 64);
    oversize.resize(512 * 1024 + 1, 0);
    for (media, bytes, expected) in [
        (
            "image/jpeg",
            branding::fixtures::png(64, 64),
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
        ),
        ("image/png", oversize, StatusCode::PAYLOAD_TOO_LARGE),
        (
            "image/png",
            branding::fixtures::png(8, 8),
            StatusCode::BAD_REQUEST,
        ),
    ] {
        let r = g.raw(Some(&admin), "PUT", &path, media, bytes).await;
        assert_eq!(r.status, expected);
    }
    assert_eq!(
        off.raw(
            Some(&admin),
            "PUT",
            &path,
            "image/png",
            branding::fixtures::png(64, 64)
        )
        .await
        .status,
        StatusCode::CONFLICT
    );
    let stored = value(
        &off.send(&auditor, "GET", &format!("{SETTINGS}/general"), Value::Null)
            .await,
    );
    assert!(
        stored["logo"].is_object(),
        "stored branding metadata survives store-off state"
    );
    assert_eq!(stored["logo_upload"]["available"], false);
    assert!(
        good(
            &off,
            &mut observed,
            &admin,
            "DELETE",
            &path,
            &path,
            Value::Null
        )
        .await["logo"]
            .is_null()
    );
    let path = "/api/v1/platform/server-policy";
    good(&g, &mut observed, &auditor, "GET", path, path, Value::Null).await;
    let path = "/api/v1/platform/installation/policy";
    for method in ["GET", "PUT"] {
        let body = good(
            &g,
            &mut observed,
            &admin,
            method,
            path,
            path,
            json!({"ignored":true}),
        )
        .await;
        assert_eq!(body["error"]["reason"], "installation_limits_removed");
    }

    let rule_input = json!({"name":"Contract budget","kind":"budget_threshold","budget_layers":["type","local"],"thresholds":[80],"notify_workspace_admins":false,"notify_platform_admins":true});
    let mut rules = Vec::new();
    for (prefix, template, actor) in [
        (format!("{ALERTS}/rules"), format!("{ALERTS}/rules"), &admin),
        (
            format!("/api/v1/workspaces/{}/alerts/rules", f.team),
            "/api/v1/workspaces/{ws}/alerts/rules".into(),
            &owner,
        ),
    ] {
        let rule = good(
            &g,
            &mut observed,
            actor,
            "POST",
            &prefix,
            &template,
            rule_input.clone(),
        )
        .await;
        reject_json_configuration(&g, actor, "POST", &prefix, rule_input.clone()).await;
        let id = id(&rule);
        let path = format!("{prefix}/{id}");
        let item = format!("{template}/{{id}}");
        good(&g, &mut observed, actor, "GET", &path, &item, Value::Null).await;
        assert_eq!(
            good(
                &g,
                &mut observed,
                actor,
                "GET",
                &prefix,
                &template,
                Value::Null
            )
            .await["data"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        good(
            &g,
            &mut observed,
            actor,
            "PUT",
            &path,
            &item,
            rule_input.clone(),
        )
        .await;
        reject_json_configuration(&g, actor, "PUT", &path, rule_input.clone()).await;
        let mut changed = rule_input.clone();
        changed["kind"] = json!("batch_failed");
        changed.as_object_mut().unwrap().remove("budget_layers");
        changed.as_object_mut().unwrap().remove("thresholds");
        assert_eq!(
            g.send(actor, "PUT", &path, changed).await.status,
            StatusCode::CONFLICT
        );
        rules.push((id, prefix, template));
    }
    // Persisted event and non-null delivery counters in both histories.
    for (rule, ws) in [(rules[0].0, None), (rules[1].0, Some(f.team))] {
        let event = Uuid::new_v4();
        sqlx::query("INSERT INTO alert_events(id,rule_id,kind,subject_key,level,severity,workspace_id,summary,details) VALUES($1,$2,'budget_threshold',$1::text,80,'warning',$3,'Budget threshold',jsonb_build_object('used_microusd','9007199254740993'))")
            .bind(event).bind(rule).bind(ws).execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO alert_deliveries(id,event_id,transition,status,recipients,sent,failed,completed_at) VALUES($1,$2,'fired','sent',2,2,0,now())").bind(Uuid::new_v4()).bind(event).execute(&pool).await.unwrap();
    }
    let ceiling=good(&g,&mut observed,&admin,"POST",&format!("{ALERTS}/rules"),&format!("{ALERTS}/rules"),json!({"name":"Ceiling","kind":"admission_ceiling","window_minutes":5,"ceiling_requests_per_second":200,"notify_platform_admins":true})).await;
    for personal in [false, true] {
        sqlx::query("INSERT INTO alert_events(id,rule_id,kind,subject_key,level,severity,workspace_id,summary,details) VALUES($1,$2,'admission_ceiling',$1::text,1,'warning',$3,'Admission ceiling',$4)")
            .bind(Uuid::new_v4()).bind(id(&ceiling)).bind(if personal{None}else{Some(f.team)}).bind(json!({
                "scope":"workspace", "personal":personal,
                "workspace_name":if personal{None}else{Some("Team")},
                "workspace_kind":if personal{"personal"}else{"team"},
                "window_minutes":5, "admissions":60000, "admissions_per_second":"200.0",
                "by_rate":true, "by_lock_wait":false, "threshold_requests_per_second":200,
                "lock_wait_p95_at_least_ms":null, "threshold_lock_wait_ms":null
            })).execute(&pool).await.unwrap();
    }
    let platform_events = format!("{ALERTS}/events");
    for actor in [&admin, &auditor] {
        let b = good(
            &g,
            &mut observed,
            actor,
            "GET",
            &platform_events,
            &platform_events,
            Value::Null,
        )
        .await;
        assert_eq!(b["data"].as_array().unwrap().len(), 3);
        for e in b["data"].as_array().unwrap() {
            if e["kind"] == "admission_ceiling"
                && (e["details"]["personal"] == true || std::ptr::eq(actor, &auditor))
            {
                assert!(e["workspace"].is_null());
                assert!(e["details"]["workspace_name"].is_null());
            }
        }
    }
    let path = format!("/api/v1/workspaces/{}/alerts/events", f.team);
    assert_eq!(
        good(
            &g,
            &mut observed,
            &owner,
            "GET",
            &path,
            "/api/v1/workspaces/{ws}/alerts/events",
            Value::Null
        )
        .await["data"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let personal = format!("/api/v1/workspaces/{}/alerts/rules", f.personal);
    let b = good(
        &g,
        &mut observed,
        &owner,
        "GET",
        &personal,
        "/api/v1/workspaces/{ws}/alerts/rules",
        Value::Null,
    )
    .await;
    assert_eq!(b["writable"], false);
    assert!(b["data"].as_array().unwrap().is_empty());
    assert!(b["builtin"].is_object());
    assert_eq!(
        g.send(&owner, "POST", &personal, rule_input.clone())
            .await
            .status,
        StatusCode::FORBIDDEN
    );
    for actor in [&admin, &auditor] {
        for suffix in ["rules", "events"] {
            assert_eq!(
                g.send(
                    actor,
                    "GET",
                    &format!("/api/v1/workspaces/{}/alerts/{suffix}", f.personal),
                    Value::Null
                )
                .await
                .status,
                StatusCode::NOT_FOUND
            );
        }
    }
    for (index, (id, prefix, template)) in rules.into_iter().enumerate() {
        good(
            &g,
            &mut observed,
            if index == 0 { &admin } else { &owner },
            "DELETE",
            &format!("{prefix}/{id}"),
            &format!("{template}/{{id}}"),
            Value::Null,
        )
        .await;
    }

    // Every rule kind and every incident shape: rules from real API writes, incidents
    // and deliveries exactly as the producers write them. Run before any webhook
    // destination is enabled, because a delivery row enqueues one webhook per destination.
    let connection = alerts_states::provider_connection(&pool).await;
    let platform_rules = alerts_states::Family {
        prefix: format!("{ALERTS}/rules"),
        template: format!("{ALERTS}/rules"),
        actor: &admin,
    };
    let workspace_rules = alerts_states::Family {
        prefix: format!("/api/v1/workspaces/{}/alerts/rules", f.team),
        template: "/api/v1/workspaces/{ws}/alerts/rules".into(),
        actor: &owner,
    };
    let platform_made = alerts_states::create_rules(
        &g,
        &mut observed,
        &platform_rules,
        alerts_states::platform_rules(connection),
    )
    .await;
    let workspace_made = alerts_states::create_rules(
        &g,
        &mut observed,
        &workspace_rules,
        alerts_states::workspace_rules(),
    )
    .await;
    alerts_states::platform_events(
        &g,
        &pool,
        &mut observed,
        &admin,
        &auditor,
        &alerts_states::Platform {
            rules: platform_made.clone(),
            connection,
            team: f.team,
            project: f.project,
        },
    )
    .await;
    alerts_states::workspace_events(
        &g,
        &pool,
        &mut observed,
        &owner,
        &auditor,
        &alerts_states::Workspace {
            team: f.team,
            personal: f.personal,
            rules: workspace_made.clone(),
        },
    )
    .await;
    alerts_states::refresh_rules(
        &g,
        &mut observed,
        &platform_rules,
        &[&admin, &auditor],
        &platform_made,
    )
    .await;
    alerts_states::refresh_rules(
        &g,
        &mut observed,
        &workspace_rules,
        &[&owner, &auditor],
        &workspace_made,
    )
    .await;
    let batch_failed = |made: &[(Uuid, Value)]| {
        made.iter()
            .find(|(_, b)| b["kind"] == "batch_failed")
            .map(|(id, _)| *id)
            .unwrap()
    };
    alerts_states::deleted_rule_history(
        &g,
        &mut observed,
        (&platform_rules, batch_failed(&platform_made)),
        (&workspace_rules, batch_failed(&workspace_made)),
        (&admin, &owner),
    )
    .await;

    let prefix = format!("{ALERTS}/webhooks");
    // Manual decoder accepts JSON even with text/plain, including serde arrays.
    let r = g
        .raw(
            Some(&admin),
            "POST",
            &prefix,
            "text/plain",
            serde_json::to_vec(&json!([
                "Receiver",
                "https://receiver.example.test/ingest",
                "env:HOOK_CURRENT"
            ]))
            .unwrap(),
        )
        .await;
    let created = positive(&mut observed, &r, "POST", &prefix);
    let hook = id(&created);
    let path = format!("{prefix}/{hook}");
    let template = format!("{prefix}/{{id}}");
    let until = (chrono::Utc::now() + chrono::Duration::hours(1))
        .format("%Y-%m-%d %H:%M:%SZ")
        .to_string();
    good(&g,&mut observed,&admin,"PATCH",&path,&template,json!({"enabled":true,"previous_secret_ref":"env:HOOK_PREVIOUS","previous_secret_until":until})).await;
    for (actor, privileged) in [(&admin, true), (&auditor, false)] {
        for (p, t) in [(&prefix, &prefix), (&path, &template)] {
            let b = good(&g, &mut observed, actor, "GET", p, t, Value::Null).await;
            let b = if p == &prefix { &b["data"][0] } else { &b };
            assert_eq!(b.get("url").is_some(), privileged);
            assert_eq!(b.get("current_secret_ref").is_some(), privileged);
            assert_eq!(b.get("previous_secret_ref").is_some(), privileged);
        }
    }
    for invalid in [
        json!({"label":null}),
        json!({"enabled":null}),
        json!({"current_secret_ref":null}),
        json!({"url":"https://attacker.example.test"}),
        json!({"nested":{"unexpected":true}}),
    ] {
        assert_eq!(
            g.send(&admin, "PATCH", &path, invalid).await.status,
            StatusCode::BAD_REQUEST
        );
    }
    good(
        &g,
        &mut observed,
        &admin,
        "PATCH",
        &path,
        &template,
        json!({"previous_secret_ref":null,"previous_secret_until":null}),
    )
    .await;
    // A read-only token owned by an Admin is still a redacted reader; adding
    // settings:write changes only the privileged reference projection.
    for (scopes, privileged) in [
        (vec!["platform:read".into()], false),
        (vec!["platform:read".into(), "settings:write".into()], true),
    ] {
        let mut tx = crate::db::begin(&pool).await.unwrap();
        crate::governance::locks::exclusive(
            &mut tx,
            [crate::governance::locks::Scope::User(f.admin.user_id)],
        )
        .await
        .unwrap();
        let token = crate::admin_tokens::create_admin_token(
            &mut tx,
            f.admin.user_id,
            "Contract reference reader",
            &scopes,
            30,
            None,
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
        for (concrete, declared) in [(&prefix, &prefix), (&path, &template)] {
            let r = capture_http(
                "GET",
                concrete,
                g.client
                    .get(format!("{}{concrete}", g.base))
                    .bearer_auth(&token.token)
                    .send()
                    .await
                    .unwrap(),
            )
            .await;
            let b = positive(&mut observed, &r, "GET", declared);
            let b = if concrete == &prefix {
                &b["data"][0]
            } else {
                &b
            };
            assert_eq!(b.get("current_secret_ref").is_some(), privileged);
            assert_eq!(b.get("url").is_some(), privileged);
        }
    }
    let test = format!("{path}/test");
    let test_template = format!("{template}/test");
    for _ in 0..2 {
        good(
            &g,
            &mut observed,
            &admin,
            "POST",
            &test,
            &test_template,
            json!({}),
        )
        .await;
    }
    let r = g.send(&admin, "POST", &test, json!({})).await;
    assert_eq!(r.status, StatusCode::TOO_MANY_REQUESTS);
    assert!(!r.headers.contains_key("retry-after"));
    for (actor, privileged) in [(&admin, true), (&auditor, false)] {
        let b = good(
            &g,
            &mut observed,
            actor,
            "GET",
            &format!("{path}/deliveries"),
            &format!("{template}/deliveries"),
            Value::Null,
        )
        .await;
        assert_eq!(b["counts"]["waiting"], 2);
        assert_eq!(b["data"].as_array().unwrap().len(), 2);
        assert_eq!(b["destination"].get("url").is_some(), privileged);
        for item in b["data"].as_array().unwrap() {
            assert!(item.get("payload").is_none());
            assert!(item["attempt_count"].is_i64());
        }
    }
    good(
        &g,
        &mut observed,
        &admin,
        "DELETE",
        &path,
        &template,
        json!({}),
    )
    .await;
    assert_eq!(
        g.send(&admin, "GET", &path, Value::Null).await.status,
        StatusCode::NOT_FOUND
    );
    alerts_states::webhooks(&g, &pool, &mut observed, &admin, &auditor).await;
    // Every documented branch of the settings responses, from real state.
    let mut extra = Vec::new();
    for name in ["admin2", "admin3", "admin4", "admin5"] {
        let principal = user(&pool, name, "admin").await;
        extra.push(browser(&pool, &principal).await);
    }
    let nomail = states::admin_without_email(&pool).await;
    states::general_and_logo(&g, &mut observed, &admin, &auditor, &nomail).await;
    states::privacy(&g, &mut observed, &admin).await;
    states::email(&g, &mut observed, &admin, &extra[0], port).await;
    states::storage(
        &pool,
        &f.s,
        &mut observed,
        &admin,
        &auditor,
        [&extra[1], &extra[2], &extra[3]],
    )
    .await;
    let unconfigured = Gateway::start(
        Store::new(pool.clone()),
        &pool,
        crate::filestore::FileStoreRuntime::memory(),
    )
    .await;
    let dead = crate::audit_export::Exporter::new("http://127.0.0.1:1/ingest", vec![7; 32], true)
        .unwrap()
        .unwrap();
    let broken = Gateway::start(
        Store::new(pool.clone()).with_audit_export(std::sync::Arc::new(dead)),
        &pool,
        crate::filestore::FileStoreRuntime::memory(),
    )
    .await;
    let live = crate::audit_export::Exporter::new(&url, vec![7; 32], true)
        .unwrap()
        .unwrap();
    states::audit_export(
        &pool,
        &g,
        &unconfigured,
        &broken,
        &mut observed,
        &admin,
        &auditor,
        &extra[0],
        live,
    )
    .await;
    let scim = Gateway::start_with(
        f.s.clone(),
        &pool,
        crate::filestore::FileStoreRuntime::memory(),
        Sign::OidcScim,
    )
    .await;
    let none = Gateway::start_with(
        f.s.clone(),
        &pool,
        crate::filestore::FileStoreRuntime::memory(),
        Sign::NoProvider,
    )
    .await;
    states::sign_in(&pool, &g, &scim, &none, &mut observed, &auditor).await;
    observed.finish(openapi::responses::SLICE5_AREAS);
}
