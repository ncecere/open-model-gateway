//! #400 (Option A): a personal workspace on the platform routes. Platform Admins
//! and Auditors may see its identity, totals and platform-managed configuration,
//! never its keys, requests, sessions or owner-made choices. Real router, real
//! sessions, exact response field allowlists.
use super::*;

async fn send(
    app: &Router,
    method: &str,
    path: &str,
    session: Option<&Session>,
    bearer: Option<&str>,
    body: Value,
) -> (StatusCode, Value) {
    let mut req = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json")
        .header("origin", ORIGIN);
    if let Some(s) = session {
        req = req
            .header("cookie", format!("omg_session={}", s.cookie))
            .header("x-csrf-token", &s.csrf);
    }
    if let Some(b) = bearer {
        req = req.header("authorization", format!("Bearer {b}"));
    }
    let response = app
        .clone()
        .oneshot(req.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// Sorted object keys: the allowlist comparison is exact, so a new field on a
/// platform response fails here until it is reviewed against the rule.
fn fields(v: &Value) -> Vec<String> {
    let mut keys: Vec<String> = v.as_object().unwrap().keys().cloned().collect();
    keys.sort();
    keys
}
fn expect(v: &Value, allowed: &[&str]) {
    let mut allowed = allowed.to_vec();
    allowed.sort();
    assert_eq!(fields(v), allowed, "{v}");
}

struct Harness {
    app: Router,
    admin: Session,
    auditor: Session,
    owner: Session,
    other: Session,
    inference: String,
    /// Strings that identify the owner's keys, requests and sessions; none may
    /// appear in any platform response.
    secrets: Vec<String>,
}
impl Harness {
    async fn new(pool: &PgPool, f: &Fixture) -> Self {
        let owner = session(pool, &f.owner).await;
        // The owner's own key in the personal workspace, plus one execution
        // (a request with a client session) it made.
        let (status, key_value) = key(f, &f.owner, f.personal, Value::Null).await;
        assert_eq!(status, StatusCode::OK, "{key_value}");
        let key_id = id(&key_value);
        let model = model(pool, "personal-visibility-model").await;
        let provider = Uuid::new_v4();
        let deployment = Uuid::new_v4();
        sqlx::query("INSERT INTO provider_connections(id,name,provider,credential_ref) VALUES($1,'Visibility','openai','env:NOT_RESOLVED')")
            .bind(provider).execute(pool).await.unwrap();
        sqlx::query("INSERT INTO deployments(id,model_id,provider_connection_id,upstream_model) VALUES($1,$2,$3,'visibility-upstream')")
            .bind(deployment).bind(model).bind(provider).execute(pool).await.unwrap();
        let execution = Uuid::new_v4();
        sqlx::query("INSERT INTO inference_executions(id,workspace_id,api_key_id,deployment_id,public_model,provider,streamed,state,root_request_id,input_tokens,output_tokens,client_session_id) VALUES($1,$2,$3,$4,'personal-visibility-model','openai',false,'succeeded',$1,7,3,'personal-visibility-session')")
            .bind(execution).bind(f.personal).bind(key_id).bind(deployment).execute(pool).await.unwrap();
        let owner_session: Uuid =
            sqlx::query_scalar("SELECT id FROM browser_sessions WHERE token_hash=$1")
                .bind(Sha256::digest(owner.cookie.as_bytes()).as_slice())
                .fetch_one(pool)
                .await
                .unwrap();
        let inference = key_value["token"].as_str().unwrap().to_owned();
        let mock = MockProvider::start(None).await;
        let identity = mock.state(pool.clone()).await;
        let engine = crate::inference::Engine::new(
            std::sync::Arc::new(f.s.clone()),
            crate::providers::ProviderRegistry::default(),
            crate::inference::EngineLimits::default(),
        )
        .unwrap();
        let app = crate::http::router_with_identity(f.s.clone(), None, engine, identity);
        Self {
            app,
            admin: session(pool, &f.admin).await,
            auditor: session(pool, &f.auditor).await,
            owner,
            other: session(pool, &f.outsider).await,
            secrets: vec![
                key_id.to_string(),
                inference.clone(),
                execution.to_string(),
                owner_session.to_string(),
                "personal-visibility-session".to_owned(),
            ],
            inference,
        }
    }
    async fn as_admin(&self, method: &str, path: &str, body: Value) -> (StatusCode, Value) {
        send(&self.app, method, path, Some(&self.admin), None, body).await
    }
    /// Owner, another plain user and an inference key are refused on a platform
    /// route; none receives a body that names the workspace.
    async fn refused(&self, method: &str, path: &str, body: Value, ws: Uuid) {
        for (label, session, bearer, expected) in [
            ("owner", Some(&self.owner), None, StatusCode::FORBIDDEN),
            ("other user", Some(&self.other), None, StatusCode::FORBIDDEN),
            (
                "inference key",
                None,
                Some(self.inference.as_str()),
                StatusCode::UNAUTHORIZED,
            ),
        ] {
            let (status, reply) =
                send(&self.app, method, path, session, bearer, body.clone()).await;
            assert_eq!(status, expected, "{method} {path} {label}: {reply}");
            assert!(!reply.to_string().contains(&ws.to_string()), "{label}");
        }
    }
    fn no_private(&self, what: &str, v: &Value) {
        let text = v.to_string();
        for secret in &self.secrets {
            assert!(!text.contains(secret), "{what} leaked {secret}: {text}");
        }
    }
}

/// `GET`/`PUT /platform/workspaces/{ws}/catalogs` on a personal workspace.
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn personal_workspace_catalog_override_is_platform_managed_and_read_back(pool: PgPool) {
    let f = fixture(&pool).await;
    let h = Harness::new(&pool, &f).await;
    let path = format!("/api/v1/platform/workspaces/{}/catalogs", f.personal);
    let (status, catalog) = h
        .as_admin(
            "POST",
            "/api/v1/platform/catalogs",
            json!({"name":"Personal pick","description":null}),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let catalog = id(&catalog);
    let body = json!({"mode":"replace","catalog_ids":[catalog]});

    // Before any override: Admin and Auditor read the inherited configuration.
    for (label, s) in [("admin", &h.admin), ("auditor", &h.auditor)] {
        let (status, v) = send(&h.app, "GET", &path, Some(s), None, Value::Null).await;
        assert_eq!(status, StatusCode::OK, "{label}");
        expect(&v, &["mode", "catalog_ids", "effective_catalog_ids"]);
        assert_eq!(v["mode"], "inherit", "{label}");
        assert_eq!(v["catalog_ids"], json!([]), "{label}");
    }

    // Writes: the Auditor and every non-platform persona are refused, and
    // nothing changes.
    let (status, _) = send(&h.app, "PUT", &path, Some(&h.auditor), None, body.clone()).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "auditor write");
    h.refused("GET", &path, Value::Null, f.personal).await;
    h.refused("PUT", &path, body.clone(), f.personal).await;
    let (_, v) = h.as_admin("GET", &path, Value::Null).await;
    assert_eq!(v["mode"], "inherit");

    // The Admin sets the personal workspace's override and both platform roles
    // read it back, with exactly the platform-managed fields.
    let (status, _) = h.as_admin("PUT", &path, body).await;
    assert_eq!(status, StatusCode::OK, "admin write");
    for (label, s) in [("admin", &h.admin), ("auditor", &h.auditor)] {
        let (status, v) = send(&h.app, "GET", &path, Some(s), None, Value::Null).await;
        assert_eq!(status, StatusCode::OK, "{label}");
        expect(&v, &["mode", "catalog_ids", "effective_catalog_ids"]);
        assert_eq!(v["mode"], "replace", "{label}");
        assert_eq!(v["catalog_ids"], json!([catalog]), "{label}");
        assert_eq!(v["effective_catalog_ids"], json!([catalog]), "{label}");
        h.no_private("catalogs", &v);
    }
    // The catalog's detail only counts personal workspaces (never lists one).
    let (_, detail) = h
        .as_admin(
            "GET",
            &format!("/api/v1/platform/catalogs/{catalog}"),
            Value::Null,
        )
        .await;
    assert_eq!(
        detail["own_choice"],
        json!({"workspaces":[],"personal_count":1})
    );
    assert!(!detail.to_string().contains(&f.personal.to_string()));
}

/// Read a personal workspace's platform policy and pin its exact field set.
async fn read_policy(h: &Harness, label: &str, s: &Session, path: &str) -> Value {
    let (status, v) = send(&h.app, "GET", path, Some(s), None, Value::Null).await;
    assert_eq!(status, StatusCode::OK, "{label}");
    expect(
        &v,
        &[
            "storage",
            "policy",
            "effective",
            "mode",
            "provenance",
            "budgets",
        ],
    );
    expect(
        &v["provenance"],
        &[
            "platform_source",
            "platform",
            "local",
            "key",
            "type_default",
        ],
    );
    expect(
        &v["storage"],
        &["quota_bytes", "used_bytes", "usage_visible"],
    );
    assert_eq!(v["provenance"]["key"], Value::Null, "{label}");
    v
}

/// `GET`/`PUT /platform/workspaces/{ws}/policy` on a personal workspace.
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn personal_workspace_policy_is_platform_managed_and_read_back(pool: PgPool) {
    let f = fixture(&pool).await;
    let h = Harness::new(&pool, &f).await;
    let path = format!("/api/v1/platform/workspaces/{}/policy", f.personal);
    let body = json!({"requests_per_minute":5,"tokens_per_minute":null,"concurrent_requests":null,"monthly_budget_microusd":null});
    for (label, s) in [("admin", &h.admin), ("auditor", &h.auditor)] {
        let v = read_policy(&h, label, s, &path).await;
        assert_eq!(v["mode"], "inherit", "{label}");
        h.no_private("policy", &v);
    }

    let (status, _) = send(&h.app, "PUT", &path, Some(&h.auditor), None, body.clone()).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "auditor write");
    h.refused("GET", &path, Value::Null, f.personal).await;
    h.refused("PUT", &path, body.clone(), f.personal).await;
    assert_eq!(
        read_policy(&h, "admin", &h.admin, &path).await["mode"],
        "inherit"
    );

    let (status, _) = h.as_admin("PUT", &path, body).await;
    assert_eq!(status, StatusCode::OK, "admin write");
    for (label, s) in [("admin", &h.admin), ("auditor", &h.auditor)] {
        let v = read_policy(&h, label, s, &path).await;
        assert_eq!(v["mode"], "replace", "{label}");
        assert_eq!(v["policy"]["requests_per_minute"], 5, "{label}");
        assert_eq!(v["effective"]["requests_per_minute"], 5, "{label}");
        h.no_private("policy", &v);
    }
}

/// The platform cost report shows the personal workspace's identity and totals
/// but no key, request or session data.
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn personal_workspace_row_in_platform_cost_report_is_identity_and_totals_only(pool: PgPool) {
    let f = fixture(&pool).await;
    let h = Harness::new(&pool, &f).await;
    let today = chrono::Utc::now().date_naive();
    let path = format!(
        "/api/v1/platform/cost-report?start_date={today}&end_date={}",
        today.succ_opt().unwrap()
    );
    for (label, s) in [("admin", &h.admin), ("auditor", &h.auditor)] {
        let (status, v) = send(&h.app, "GET", &path, Some(s), None, Value::Null).await;
        assert_eq!(status, StatusCode::OK, "{label}: {v}");
        let rows = v["breakdowns"]["workspaces"].as_array().unwrap();
        let personal: Vec<_> = rows.iter().filter(|r| r["kind"] == "personal").collect();
        assert_eq!(personal.len(), 1, "{label}: {v}");
        let row = personal[0];
        assert_eq!(row["id"], f.personal.to_string(), "{label}");
        assert_eq!(row["name"], "Personal · owner@example.test", "{label}");
        expect(row, &["id", "kind", "name", "totals"]);
        expect(
            &row["totals"],
            &[
                "attempts",
                "held_microusd",
                "known_cost_microusd",
                "root_requests",
                "unresolved_attempts",
            ],
        );
        expect(
            &v["breakdowns"],
            &[
                "cost_centers",
                "models",
                "providers",
                "service_accounts",
                "workspaces",
            ],
        );
        // Totals only: no key rows, and every aggregate row is just
        // `{id, name, totals}` (no workspace, key, request or session fields).
        assert_eq!(v["breakdowns"]["service_accounts"], json!([]), "{label}");
        for dimension in ["cost_centers", "models", "providers", "workspaces"] {
            for row in v["breakdowns"][dimension].as_array().unwrap() {
                let mut allowed = vec!["id", "name", "totals"];
                if dimension == "workspaces" {
                    allowed.push("kind");
                }
                expect(row, &allowed);
            }
        }
        h.no_private("cost report", &v);
    }
    // The owner's own cost report route is theirs; the platform route is not.
    h.refused("GET", &path, Value::Null, f.personal).await;
}
