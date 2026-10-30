//! Every Slice 2 operation is executed over the real loopback serving stack.
//! Fixture credentials stay in memory; neither tokens nor response bodies are logged.
//!
//! Data comes from the real API wherever a slice 2 operation produces it and
//! from rows written exactly as the producing code writes them otherwise
//! (`lifecycle::apply_group_grants`, `disable_for_entitlement_loss`, the
//! engine's execution and reservation rows, an accepted invitation). Boundary
//! vectors run last because their accepted variants change state.
use super::*;
use futures::FutureExt;
use openapi::http_vectors::{
    Probe, body_vectors_http, query_vectors, query_vectors_extended, query_vectors_fixed,
};
use openapi::responses::{Captured, PositiveFixtures, SLICE2_AREAS, capture_http};
use sha2::{Digest, Sha256};
use std::cell::RefCell;

const ORIGIN: &str = "http://127.0.0.1:3000";
const ISSUER: &str = "https://issuer.example.test";
/// Handler-derived probes run by the fixture, and how many of the expected
/// accepts the handler gives as a domain refusal (404/409) rather than 2xx.
const EXPECTED_PINS: usize = 147;
const EXPECTED_DOMAIN_PINS: usize = 1;
struct Server(tokio::task::JoinHandle<()>);
impl Drop for Server {
    fn drop(&mut self) {
        self.0.abort();
    }
}
struct Browser {
    token: String,
    csrf: String,
}
async fn browser(pool: &PgPool, user: &BrowserPrincipal) -> Browser {
    let token = crate::entropy::hex_token().unwrap();
    let csrf = crate::entropy::hex_token().unwrap();
    sqlx::query("INSERT INTO browser_sessions(token_hash,user_id,csrf_hash,expires_at,verified_email,last_seen_at) VALUES($1,$2,$3,now()+interval '1 hour',$4,now())")
        .bind(Sha256::digest(token.as_bytes()).as_slice()).bind(user.user_id)
        .bind(Sha256::digest(csrf.as_bytes()).as_slice()).bind(&user.email).execute(pool).await.unwrap();
    Browser { token, csrf }
}
fn target(path: &str, query: &str) -> String {
    if query.is_empty() {
        path.to_owned()
    } else {
        format!("{path}?{query}")
    }
}
/// Percent-encode everything outside the RFC 3986 unreserved set.
fn encode_query(pairs: &[(String, String)]) -> String {
    fn escape(text: &str) -> String {
        text.bytes()
            .map(|b| match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                    char::from(b).to_string()
                }
                _ => format!("%{b:02X}"),
            })
            .collect()
    }
    pairs
        .iter()
        .map(|(n, v)| format!("{}={}", escape(n), escape(v)))
        .collect::<Vec<_>>()
        .join("&")
}
/// A policy body: the three required rates (null) plus `extra`.
fn policy_body(extra: Value) -> Value {
    let mut body =
        json!({"requests_per_minute":null,"tokens_per_minute":null,"concurrent_requests":null});
    for (name, value) in extra.as_object().unwrap() {
        body[name] = value.clone();
    }
    body
}
fn pairs(values: &[(&str, &str)]) -> Vec<(String, String)> {
    values
        .iter()
        .map(|(n, v)| ((*n).to_owned(), (*v).to_owned()))
        .collect()
}
struct Client {
    http: reqwest::Client,
    base: String,
    observed: tokio::sync::Mutex<PositiveFixtures>,
    failures: RefCell<Vec<String>>,
    /// Handler-derived probes executed (a guard against a vacuous set).
    pins: std::cell::Cell<usize>,
    /// Probes the handler accepted only through a domain refusal (404/409).
    domain_pins: std::cell::Cell<usize>,
}
impl Client {
    /// One request, captured and schema-checked against its documented
    /// operation, with no status or coverage bookkeeping (vectors and setup).
    async fn raw(
        &self,
        who: &Browser,
        method: &str,
        path: &str,
        input: Option<Value>,
        headers: &[(&str, &str)],
    ) -> Captured {
        let mut request = self
            .http
            .request(method.parse().unwrap(), format!("{}{path}", self.base))
            .header("origin", ORIGIN)
            .header("cookie", format!("omg_session={}", who.token))
            .header("x-csrf-token", &who.csrf);
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        if let Some(input) = input {
            request = request.json(&input);
        }
        capture_http(method, path, request.send().await.unwrap()).await
    }
    async fn send_with(
        &self,
        who: &Browser,
        method: &str,
        path: &str,
        input: Option<Value>,
        expected: StatusCode,
        headers: &[(&str, &str)],
    ) -> Value {
        if let Some(input) = &input
            && expected == StatusCode::OK
        {
            crate::management::openapi_semantic::check_request(method, path, input).unwrap();
        }
        let r = self.raw(who, method, path, input, headers).await;
        assert_eq!(
            r.status.as_u16(),
            expected.as_u16(),
            "{method}: unexpected status"
        );
        self.observed.lock().await.observe(&r);
        serde_json::from_slice(&r.bytes).unwrap_or(Value::Null)
    }
    async fn send(
        &self,
        who: &Browser,
        method: &str,
        path: &str,
        input: Option<Value>,
        expected: StatusCode,
    ) -> Value {
        self.send_with(who, method, path, input, expected, &[])
            .await
    }
    async fn ok(&self, who: &Browser, method: &str, path: &str, input: Option<Value>) -> Value {
        self.send(who, method, path, input, StatusCode::OK).await
    }
    /// Fixture preparation through an operation owned by another slice or
    /// area: executed for real, never counted toward this slice's coverage.
    async fn setup(&self, who: &Browser, method: &str, path: &str, input: Option<Value>) -> Value {
        let r = self.raw(who, method, path, input, &[]).await;
        assert_eq!(r.status, StatusCode::OK, "{method}: setup status");
        serde_json::from_slice(&r.bytes).unwrap_or(Value::Null)
    }
    /// A refused body, pinned against the documented request schema: the
    /// schema's verdict must equal the handler's class (`schema_accepts`).
    /// Only the verdict is compared; the body is never printed.
    async fn refused(
        &self,
        who: &Browser,
        method: &str,
        path: &str,
        body: Value,
        status: StatusCode,
        schema_accepts: bool,
    ) {
        assert_eq!(
            crate::management::openapi_semantic::check_request(method, path, &body).is_ok(),
            schema_accepts,
            "{method} {status}: request schema verdict disagrees with the handler class"
        );
        self.send(who, method, path, Some(body), status).await;
    }
    /// The handler's verdict on an input: 400 and 422 refuse it; a success, a
    /// redirect or a domain refusal (404, 409) got past input validation.
    fn input_verdict(status: StatusCode) -> Option<bool> {
        match status.as_u16() {
            400 | 422 => Some(false),
            200..=399 | 404 | 409 => Some(true),
            _ => None,
        }
    }
    /// One handler-derived probe the derived vectors cannot generate: the
    /// SCHEMA's verdict and the HANDLER's verdict on the same body, each pinned
    /// to what the handler source says. A documented divergence (the schema
    /// stricter or looser than the handler) is pinned too, so a change on either
    /// side shows up. Only the label is ever reported, never a value.
    #[allow(clippy::too_many_arguments)]
    async fn pin_body(
        &self,
        who: &Browser,
        method: &str,
        path: &str,
        what: &str,
        body: Value,
        schema_accepts: bool,
        handler_accepts: bool,
    ) {
        self.pins.set(self.pins.get() + 1);
        let schema =
            crate::management::openapi_semantic::check_request(method, path, &body).is_ok();
        let r = self.raw(who, method, path, Some(body), &[]).await;
        self.expect(
            schema == schema_accepts,
            &format!("{method} {what}: the schema verdict changed"),
        );
        self.expect(
            Self::input_verdict(r.status) == Some(handler_accepts),
            &format!("{method} {what}: the handler verdict changed"),
        );
        self.count_domain(&r, handler_accepts);
    }
    /// [`Client::pin_body`] for a GET whose query values are checked against
    /// the documented parameter schemas of `template`.
    #[allow(clippy::too_many_arguments)]
    async fn pin_query(
        &self,
        who: &Browser,
        template: &str,
        path: &str,
        what: &str,
        query: &[(&str, &str)],
        schema_accepts: bool,
        handler_accepts: bool,
    ) {
        self.pins.set(self.pins.get() + 1);
        let document = openapi::responses::artifact();
        let parameters = document["paths"][template]["get"]["parameters"]
            .as_array()
            .unwrap();
        let schema = query.iter().all(|(name, value)| {
            let parameter = parameters.iter().find(|p| p["name"] == *name).unwrap();
            openapi::responses::validate_value(document, &parameter["schema"], &json!(value))
                .is_ok()
        });
        let query: Vec<(String, String)> = query
            .iter()
            .map(|(n, v)| ((*n).to_owned(), (*v).to_owned()))
            .collect();
        let r = self
            .raw(who, "GET", &target(path, &encode_query(&query)), None, &[])
            .await;
        self.expect(
            schema == schema_accepts,
            &format!("GET {what}: the schema verdict changed"),
        );
        self.expect(
            Self::input_verdict(r.status) == Some(handler_accepts),
            &format!("GET {what}: the handler verdict changed"),
        );
        self.count_domain(&r, handler_accepts);
    }
    /// An expected accept that the handler gave only as a domain refusal
    /// (404/409): counted, and the total is pinned exactly at the end, so a
    /// vanished target (every accept turning into 404) cannot pass silently.
    fn count_domain(&self, r: &openapi::responses::Captured, handler_accepts: bool) {
        if handler_accepts && matches!(r.status.as_u16(), 404 | 409) {
            self.domain_pins.set(self.domain_pins.get() + 1);
        }
    }
    /// A fixture expectation that is reported with the rest instead of
    /// stopping the run.
    fn expect(&self, holds: bool, what: &str) {
        if !holds {
            self.failures
                .borrow_mut()
                .push(format!("expectation: {what}"));
        }
    }
    /// Run one boundary-vector set, recording (never printing) a failure so a
    /// single run reports every disagreeing operation.
    async fn guarded(&self, label: &str, work: impl std::future::Future<Output = ()>) {
        if let Err(panic) = std::panic::AssertUnwindSafe(work).catch_unwind().await {
            let message = panic
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| panic.downcast_ref::<&str>().map(|s| (*s).to_owned()))
                .unwrap_or_else(|| "panicked".to_owned());
            self.failures
                .borrow_mut()
                .push(format!("{label}: {message}"));
        }
    }
}

/// Create one mapping through the real API and return its id.
async fn create_mapping_row(c: &Client, admin: &Browser, body: Value) -> Uuid {
    id(&c
        .ok(
            admin,
            "POST",
            "/api/v1/platform/oidc/group-mappings",
            Some(body),
        )
        .await)
}
fn render(filters: &[(&str, String)], extra: &[(&str, &str)]) -> String {
    filters
        .iter()
        .map(|(n, v)| (*n, v.as_str()))
        .chain(extra.iter().copied())
        .map(|(n, v)| format!("{n}={v}"))
        .collect::<Vec<_>>()
        .join("&")
}

async fn plain_user(pool: &PgPool, name: &str) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO users(id,email) VALUES($1,$2)")
        .bind(id)
        .bind(format!("{name}@example.test"))
        .execute(pool)
        .await
        .unwrap();
    id
}
/// The profile fields a sign-in records.
async fn profile(pool: &PgPool, id: Uuid, display_name: Option<&str>, signed_in: bool) {
    sqlx::query("UPDATE users SET display_name=$2,first_signed_in_at=CASE WHEN $3 THEN now()-interval '3 days' END,last_signed_in_at=CASE WHEN $3 THEN now()-interval '1 hour' END WHERE id=$1")
        .bind(id)
        .bind(display_name)
        .bind(signed_in)
        .execute(pool)
        .await
        .unwrap();
}
/// `lifecycle::apply_group_grants`' platform insert.
async fn group_role(pool: &PgPool, user: Uuid, role: &str, mapping: Uuid) {
    sqlx::query("INSERT INTO platform_role_grants(id,user_id,role,source,mapping_id) VALUES(gen_random_uuid(),$1,$2,'group',$3)")
        .bind(user)
        .bind(role)
        .bind(mapping)
        .execute(pool)
        .await
        .unwrap();
}
/// `lifecycle::apply_group_grants`' workspace insert.
async fn group_member(pool: &PgPool, user: Uuid, ws: Uuid, role: &str, mapping: Uuid) {
    sqlx::query("INSERT INTO workspace_membership_grants(id,workspace_id,user_id,role,source,mapping_id) VALUES(gen_random_uuid(),$1,$2,$3,'group',$4)")
        .bind(ws)
        .bind(user)
        .bind(role)
        .bind(mapping)
        .execute(pool)
        .await
        .unwrap();
}
/// One engine attempt of a client session: the execution row and its
/// reservation, as the engine writes them. `settled` carries tokens and cost,
/// otherwise the usage is unknown (null tokens, no actual cost).
#[allow(clippy::too_many_arguments)]
async fn attempt(
    pool: &PgPool,
    ws: Uuid,
    key: Uuid,
    deployment: Uuid,
    session: &str,
    app: Option<&str>,
    settled: bool,
) -> Uuid {
    let execution = Uuid::new_v4();
    let tokens: (Option<i64>, Option<i64>) = if settled {
        (Some(10), Some(5))
    } else {
        (None, None)
    };
    sqlx::query("INSERT INTO inference_executions(id,workspace_id,api_key_id,deployment_id,public_model,provider,streamed,state,root_request_id,client_session_id,client_app,finish_reason,input_tokens,output_tokens,completed_at,elapsed_ms) VALUES($1,$2,$3,$4,'contract-access-model','openai',false,'succeeded',$1,$5,$6,'stop',$7,$8,clock_timestamp(),250)")
        .bind(execution).bind(ws).bind(key).bind(deployment).bind(session).bind(app).bind(tokens.0).bind(tokens.1)
        .execute(pool).await.unwrap();
    if settled {
        sqlx::query("INSERT INTO governance_reservations(execution_id,workspace_id,api_key_id,deployment_id,admitted_at,minute_start,month_start,lease_expires_at,state,actual_microusd,unbounded_cost,input_tokens,output_tokens) SELECT $1,$2,$3,$4,e.started_at,date_trunc('minute',e.started_at,'UTC'),date_trunc('month',e.started_at,'UTC'),e.started_at,'settled',7,false,10,5 FROM inference_executions e WHERE e.id=$1")
            .bind(execution).bind(ws).bind(key).bind(deployment)
            .execute(pool).await.unwrap();
    }
    execution
}

/// A failed attempt whose cost is unknown (the engine's `unknown` reservation
/// with an unbounded cost): its budget windows can no longer state a remainder.
async fn unknown_cost_attempt(pool: &PgPool, ws: Uuid, key: Uuid, deployment: Uuid) {
    let execution = Uuid::new_v4();
    sqlx::query("INSERT INTO inference_executions(id,workspace_id,api_key_id,deployment_id,public_model,provider,streamed,state,error_code,root_request_id,completed_at,elapsed_ms) VALUES($1,$2,$3,$4,'contract-access-model','openai',false,'failed','upstream_unavailable',$1,clock_timestamp(),250)")
        .bind(execution).bind(ws).bind(key).bind(deployment)
        .execute(pool).await.unwrap();
    sqlx::query("INSERT INTO governance_reservations(execution_id,workspace_id,api_key_id,deployment_id,admitted_at,minute_start,month_start,lease_expires_at,state,unbounded_cost) SELECT $1,$2,$3,$4,e.started_at,date_trunc('minute',e.started_at,'UTC'),date_trunc('month',e.started_at,'UTC'),e.started_at,'unknown',true FROM inference_executions e WHERE e.id=$1")
        .bind(execution).bind(ws).bind(key).bind(deployment)
        .execute(pool).await.unwrap();
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn management_openapi_slice2_fixtures(pool: PgPool) {
    let f = fixture(&pool).await;
    let admin = browser(&pool, &f.admin).await;
    let auditor = browser(&pool, &f.auditor).await;
    let owner = browser(&pool, &f.owner).await;
    let regular = browser(&pool, &f.member).await;
    let outsider = browser(&pool, &f.outsider).await;
    let foreign = workspace(&pool, "Private foreign", "personal", f.outsider.user_id).await;
    let mock = crate::identity::tests::MockProvider::start(None).await;
    let identity = mock.state(pool.clone()).await;
    let engine = crate::inference::Engine::new(
        std::sync::Arc::new(f.s.clone()),
        crate::providers::ProviderRegistry::default(),
        crate::inference::EngineLimits::default(),
    )
    .unwrap();
    let app = crate::http::router_with_identity(f.s.clone(), None, engine, identity);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let _server = Server(tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap()
    }));
    let c = Client {
        http: reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap(),
        base,
        observed: tokio::sync::Mutex::new(PositiveFixtures::default()),
        failures: RefCell::new(Vec::new()),
        pins: std::cell::Cell::new(0),
        domain_pins: std::cell::Cell::new(0),
    };
    let c = &c;

    // Profiles as a sign-in records them, and one cost center (platform-owned).
    profile(&pool, f.member.user_id, Some("Member Person"), true).await;
    profile(&pool, f.owner.user_id, Some("Owner Person"), true).await;
    let cost_center = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO cost_centers(id,name,code) VALUES($1,'Contract cost center','CC-100')",
    )
    .bind(cost_center)
    .execute(&pool)
    .await
    .unwrap();

    // Shared workspaces: the main one, a second active one and one that gets disabled.
    let mut shared = Vec::new();
    for (name, kind) in [
        ("Contract shared", "project"),
        ("Contract second", "team"),
        ("Contract disabled", "project"),
    ] {
        let created = c
            .ok(
                &admin,
                "POST",
                "/api/v1/platform/workspaces",
                Some(json!({"name":name,"kind":kind,"owner_user_id":f.owner.user_id})),
            )
            .await;
        shared.push(id(&created));
    }
    let (ws, ws2, wsd) = (shared[0], shared[1], shared[2]);
    let platform = format!("/api/v1/platform/workspaces/{ws}");
    let own = format!("/api/v1/workspaces/{ws}");

    // User directory, lifecycle and role provenance. The last Admin is protected.
    let created = c
        .ok(
            &admin,
            "POST",
            "/api/v1/platform/users",
            Some(json!({"email":"contract-new@example.test","platform_role":"user"})),
        )
        .await;
    let new_user = id(&created);
    let up = format!("/api/v1/platform/users/{new_user}");
    c.ok(
        &admin,
        "POST",
        &format!("{up}/roles"),
        Some(json!({"role":"auditor"})),
    )
    .await;
    c.ok(&admin, "DELETE", &format!("{up}/roles/auditor"), None)
        .await;
    c.ok(
        &admin,
        "PATCH",
        &up,
        Some(json!({"oidc_link_allowed":false})),
    )
    .await;
    // A shared membership that cleanup later revokes (an export row with revoked_at).
    c.ok(
        &admin,
        "POST",
        &format!("{platform}/members"),
        Some(json!({"user_id":new_user,"role":"member"})),
    )
    .await;
    c.ok(
        &admin,
        "POST",
        &format!(
            "/api/v1/platform/users/{}/sessions/revoke",
            f.member.user_id
        ),
        None,
    )
    .await;
    // Use a renewed browser after revocation, proving the revoked credential is refused.
    c.send(
        &regular,
        "GET",
        &format!("/api/v1/workspaces/{}", f.team),
        None,
        StatusCode::UNAUTHORIZED,
    )
    .await;
    let regular = browser(&pool, &f.member).await;
    c.send(
        &admin,
        "DELETE",
        &format!("/api/v1/platform/users/{}/roles/admin", f.admin.user_id),
        None,
        StatusCode::CONFLICT,
    )
    .await;
    c.send(
        &admin,
        "PATCH",
        &format!("/api/v1/platform/users/{}", f.owner.user_id),
        Some(json!({"disabled":true})),
        StatusCode::CONFLICT,
    )
    .await;
    c.send(
        &admin,
        "POST",
        &format!("{up}/remove-personal-data"),
        None,
        StatusCode::CONFLICT,
    )
    .await;
    c.ok(&admin, "PATCH", &up, Some(json!({"disabled":true})))
        .await;
    c.ok(&admin, "POST", &format!("{up}/remove-personal-data"), None)
        .await;
    assert!(c.ok(&auditor, "GET", &up, None).await["email"].is_null());
    c.send(
        &auditor,
        "PATCH",
        &up,
        Some(json!({})),
        StatusCode::FORBIDDEN,
    )
    .await;

    // Shared and owner-private workspace roots are different closed representations.
    assert!(
        !c.ok(&auditor, "GET", "/api/v1/platform/workspaces", None)
            .await["data"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    c.ok(&auditor, "GET", &platform, None).await;
    c.ok(
        &admin,
        "PATCH",
        &platform,
        Some(json!({"name":"Contract renamed","disabled":false,"cost_center_id":cost_center})),
    )
    .await;
    // The platform-owned allocation applies to a personal workspace too (#400).
    let personal_path = format!("/api/v1/workspaces/{}", f.personal);
    let before = c.ok(&owner, "GET", &personal_path, None).await;
    assert!(before["cost_center"].is_null());
    c.ok(
        &admin,
        "PATCH",
        &format!("/api/v1/platform/workspaces/{}", f.personal),
        Some(json!({"cost_center_id":cost_center})),
    )
    .await;
    c.ok(
        &owner,
        "PATCH",
        &own,
        Some(json!({"name":"Owner renamed","object_visibility":"workspace"})),
    )
    .await;
    let context = c.ok(&owner, "GET", &own, None).await;
    assert_eq!(context["role"], "owner");
    assert_eq!(context["object_visibility"], "workspace");
    assert!(context["cost_center"].is_object());
    c.ok(
        &owner,
        "GET",
        &format!("/api/v1/workspaces/{}", f.project),
        None,
    )
    .await;
    assert!(c.ok(&auditor, "GET", &own, None).await["role"].is_null());
    let personal = c.ok(&owner, "GET", &personal_path, None).await;
    assert!(personal.get("object_visibility").is_none());
    assert!(personal["cost_center"].is_object());
    for principal in [&admin, &auditor, &owner] {
        c.send(
            principal,
            "GET",
            &format!("/api/v1/workspaces/{foreign}"),
            None,
            StatusCode::NOT_FOUND,
        )
        .await;
        c.send(
            principal,
            "GET",
            &format!("/api/v1/platform/workspaces/{foreign}"),
            None,
            if std::ptr::eq(principal, &owner) {
                StatusCode::FORBIDDEN
            } else {
                StatusCode::NOT_FOUND
            },
        )
        .await;
    }
    c.send(
        &owner,
        "PATCH",
        &personal_path,
        Some(json!({"name":"not allowed"})),
        StatusCode::CONFLICT,
    )
    .await;
    c.send(&outsider, "GET", &own, None, StatusCode::FORBIDDEN)
        .await;

    // An entitled non-member who never set a display name (the picker row's null side).
    user(&pool, "member-unnamed", "user").await;
    // Candidate pickers are populated before adding a manual member.
    for path in [
        format!("{own}/member-candidates?q=member"),
        format!("{platform}/member-candidates?q=member"),
    ] {
        let who = if path.contains("/platform/") {
            &auditor
        } else {
            &owner
        };
        assert!(
            !c.ok(who, "GET", &path, None).await["data"]
                .as_array()
                .unwrap()
                .is_empty()
        );
    }
    c.ok(
        &admin,
        "POST",
        &format!("{platform}/members"),
        Some(json!({"user_id":f.member.user_id,"role":"member"})),
    )
    .await;
    c.ok(
        &owner,
        "POST",
        &format!("{own}/members"),
        Some(json!({"user_id":f.member.user_id,"role":"admin"})),
    )
    .await;

    // Mapping CRUD and both preview kinds, with real mixed-source grants.
    let mapping_body = json!({"issuer":ISSUER,"group_value":"contract-group","target_kind":"workspace","platform_role":null,"workspace_id":ws,"workspace_role":"member","enabled":true});
    let mapping = id(&c
        .ok(
            &admin,
            "POST",
            "/api/v1/platform/oidc/group-mappings",
            Some(mapping_body.clone()),
        )
        .await);
    // Raw Json<Value> PATCH/preview bodies are manually decoded, not DTOs.
    // Validate the actual parser boundary with a successful no-op control first.
    let mapping_path = format!("/api/v1/platform/oidc/group-mappings/{mapping}");
    c.ok(&admin, "PATCH", &mapping_path, Some(json!({}))).await;
    c.send(
        &admin,
        "PATCH",
        &mapping_path,
        None,
        StatusCode::UNSUPPORTED_MEDIA_TYPE,
    )
    .await;
    // Every refusal below is also refused by the documented schema.
    for (probe, status) in [
        (
            json!({"unexpected_configuration":true}),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            json!({"enabled":{"unexpected_configuration":true}}),
            StatusCode::BAD_REQUEST,
        ),
        (json!({"enabled":null}), StatusCode::BAD_REQUEST),
        (json!({"enabled":"true"}), StatusCode::BAD_REQUEST),
        (json!({"issuer":null}), StatusCode::BAD_REQUEST),
        (json!({"group_value":null}), StatusCode::BAD_REQUEST),
        (json!({"target_kind":null}), StatusCode::BAD_REQUEST),
        (json!({"target_kind":"bogus"}), StatusCode::BAD_REQUEST),
        (
            json!({"workspace_id":"not-a-uuid"}),
            StatusCode::BAD_REQUEST,
        ),
        (json!([]), StatusCode::BAD_REQUEST),
        (Value::Null, StatusCode::BAD_REQUEST),
    ] {
        c.refused(&admin, "PATCH", &mapping_path, probe, status, false)
            .await;
    }
    let preview_path = format!("{mapping_path}/preview");
    c.ok(&admin, "POST", &preview_path, Some(json!({}))).await;
    c.send(
        &admin,
        "POST",
        &preview_path,
        None,
        StatusCode::UNSUPPORTED_MEDIA_TYPE,
    )
    .await;
    for (probe, status) in [
        (
            json!({"unexpected_configuration":true}),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (json!({"delete":null}), StatusCode::BAD_REQUEST),
        (json!({"delete":[]}), StatusCode::BAD_REQUEST),
        (json!({"delete":"true"}), StatusCode::BAD_REQUEST),
        (json!({"enabled":null}), StatusCode::BAD_REQUEST),
        (json!({"issuer":null}), StatusCode::BAD_REQUEST),
        (
            json!({"enabled":{"unexpected_configuration":true}}),
            StatusCode::BAD_REQUEST,
        ),
        (json!({"target_kind":"bogus"}), StatusCode::BAD_REQUEST),
        (json!([]), StatusCode::BAD_REQUEST),
        (Value::Null, StatusCode::BAD_REQUEST),
    ] {
        c.refused(&admin, "POST", &preview_path, probe, status, false)
            .await;
    }
    // The NEW-mapping preview is a full replacement like create: a null or
    // missing required value is refused by the handler and by the schema.
    for field in ["issuer", "group_value", "target_kind", "enabled"] {
        let mut body = json!({"issuer":ISSUER,"group_value":"contract-group","target_kind":"platform","platform_role":"user","workspace_id":null,"workspace_role":null,"enabled":true});
        body[field] = Value::Null;
        c.refused(
            &admin,
            "POST",
            "/api/v1/platform/oidc/group-mappings/preview",
            body,
            StatusCode::BAD_REQUEST,
            false,
        )
        .await;
    }
    // object_visibility is also manually validated after OwnWorkspaceUpdate
    // deserialization. Omission/null are distinct from invalid raw JSON values.
    c.ok(
        &owner,
        "PATCH",
        &own,
        Some(json!({"name":"Owner renamed","object_visibility":null})),
    )
    .await;
    // The handler's `object_visibility: Option<Value>` accepts any JSON and then
    // refuses all but "member"/"workspace"; the schema's enum rejects them too.
    for probe in [
        json!(true),
        json!(1),
        json!("wrong"),
        json!([]),
        json!({}),
        json!({"unexpected_configuration":true}),
    ] {
        c.refused(
            &owner,
            "PATCH",
            &own,
            json!({"object_visibility":probe}),
            StatusCode::BAD_REQUEST,
            false,
        )
        .await;
    }
    // Bodies that name nothing to change (null is the same as omission) are
    // refused by the handler (directory.rs:406-408) and by the schema, which has
    // one arm per way of giving a name or a visibility.
    for body in [
        json!({}),
        json!({"object_visibility":null}),
        json!({"name":null}),
        json!({"name":null,"object_visibility":null}),
    ] {
        c.refused(&owner, "PATCH", &own, body, StatusCode::BAD_REQUEST, false)
            .await;
    }
    c.refused(
        &owner,
        "PATCH",
        &own,
        json!({"unexpected_configuration":true}),
        StatusCode::UNPROCESSABLE_ENTITY,
        false,
    )
    .await;
    sqlx::query("INSERT INTO workspace_membership_grants(workspace_id,user_id,role,source,mapping_id) VALUES($1,$2,'member','group',$3)").bind(ws).bind(f.member.user_id).bind(mapping).execute(&pool).await.unwrap();
    // A member whose account is then suspended: listed with a null effective role.
    let suspended_member = user(&pool, "suspended-member", "user").await;
    profile(
        &pool,
        suspended_member.user_id,
        Some("Suspended Member"),
        true,
    )
    .await;
    c.ok(
        &owner,
        "POST",
        &format!("{own}/members"),
        Some(json!({"user_id":suspended_member.user_id,"role":"member"})),
    )
    .await;
    c.ok(
        &admin,
        "PATCH",
        &format!("/api/v1/platform/users/{}", suspended_member.user_id),
        Some(json!({"disabled":true})),
    )
    .await;
    for (who, path) in [
        (&owner, format!("{own}/members")),
        (&auditor, format!("{platform}/members")),
    ] {
        let value = c.ok(who, "GET", &path, None).await;
        let member = value["data"]
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["user_id"] == json!(f.member.user_id))
            .unwrap();
        assert_eq!(member["membership_source"], "mixed");
        assert_eq!(member["grants"].as_array().unwrap().len(), 2);
        let disabled = value["data"]
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["user_id"] == json!(suspended_member.user_id))
            .unwrap();
        assert!(disabled["role"].is_null());
        assert!(!disabled["disabled_at"].is_null());
    }

    // Group-derived access as the producers write it: platform mappings, workspace
    // mappings sharing a group, a user suspended for entitlement loss, and
    // holders with and without profile names.
    let m_pl = create_mapping_row(c, &admin, json!({"issuer":ISSUER,"group_value":"pl-group","target_kind":"platform","platform_role":"auditor","workspace_id":null,"workspace_role":null,"enabled":true})).await;
    let m_wsp = create_mapping_row(c, &admin, json!({"issuer":ISSUER,"group_value":"pl-group","target_kind":"workspace","platform_role":null,"workspace_id":ws2,"workspace_role":"member","enabled":true})).await;
    let m_wsa = create_mapping_row(c, &admin, json!({"issuer":ISSUER,"group_value":"ws-group","target_kind":"workspace","platform_role":null,"workspace_id":ws,"workspace_role":"member","enabled":true})).await;
    let m_wsb = create_mapping_row(c, &admin, json!({"issuer":ISSUER,"group_value":"ws-admin-group","target_kind":"workspace","platform_role":null,"workspace_id":ws,"workspace_role":"admin","enabled":true})).await;
    // B and B2 hold only the group's platform role (B has a name, B2 none); C also a manual role.
    let b = plain_user(&pool, "group-b").await;
    profile(&pool, b, Some("Bea Auditor"), true).await;
    let b2 = plain_user(&pool, "group-b2").await;
    let cc = user(&pool, "group-c", "user").await;
    profile(&pool, cc.user_id, Some("Cy Manual"), false).await;
    for who in [b, b2, cc.user_id] {
        group_role(&pool, who, "auditor", m_pl).await;
    }
    for who in [b, b2] {
        member(&pool, ws, who, "member").await;
    }
    // Suspended for entitlement loss (`disable_for_entitlement_loss`), still holding
    // a workspace group grant from a sign-in that matched only the workspace mapping.
    // The first user id of the roster: test builds cap a group roster at 3
    // members (group_mapping_preview.rs ROSTER_LIMIT) in user-id order.
    let d = Uuid::from_u128(1);
    sqlx::query("INSERT INTO users(id,email) VALUES($1,'group-d@example.test')")
        .bind(d)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE users SET disabled_at=now(),cleanup_due_at=now()+interval '30 days',disable_reason='entitlement_loss' WHERE id=$1").bind(d).execute(&pool).await.unwrap();
    group_member(&pool, d, ws2, "member", m_wsp).await;
    let a2 = user(&pool, "group-a2", "user").await;
    profile(&pool, a2.user_id, Some("Ann Member"), false).await;
    let e = user(&pool, "group-e", "user").await;
    member(&pool, ws2, e.user_id, "member").await;
    let fu = user(&pool, "group-f", "user").await;
    for who in [a2.user_id, e.user_id] {
        group_member(&pool, who, ws, "member", m_wsa).await;
    }
    group_member(&pool, fu.user_id, ws, "admin", m_wsb).await;
    // B belongs to a team that is later disabled (a disabled_at in the user's memberships).
    member(&pool, wsd, b, "member").await;
    c.ok(
        &admin,
        "PATCH",
        &format!("/api/v1/platform/workspaces/{wsd}"),
        Some(json!({"disabled":true})),
    )
    .await;

    assert!(
        !c.ok(
            &auditor,
            "GET",
            "/api/v1/platform/oidc/group-mappings",
            None
        )
        .await["data"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    // NEW mapping previews: a platform role for the pl-group roster, then a workspace role.
    let new_preview = "/api/v1/platform/oidc/group-mappings/preview";
    let preview = c.ok(&admin, "POST", new_preview, Some(json!({"issuer":ISSUER,"group_value":"pl-group","target_kind":"platform","platform_role":"admin","workspace_id":null,"workspace_role":null,"enabled":true}))).await;
    assert!(preview["digest"].is_null());
    c.expect(
        !preview["diff"]["platform_role_gained"]
            .as_array()
            .unwrap()
            .is_empty(),
        "new platform preview gains a platform role",
    );
    c.expect(
        !preview["diff"]["workspace_membership_gained"]
            .as_array()
            .unwrap()
            .is_empty(),
        "new platform preview reactivates a workspace membership",
    );
    let preview = c.ok(&admin, "POST", new_preview, Some(json!({"issuer":ISSUER,"group_value":"ws-group","target_kind":"workspace","platform_role":null,"workspace_id":ws2,"workspace_role":"admin","enabled":true}))).await;
    assert!(
        !preview["diff"]["workspace_membership_gained"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    // Existing-mapping previews. A delete first: digest and the exact If-Match flow.
    let existing = |m: Uuid| format!("/api/v1/platform/oidc/group-mappings/{m}");
    let preview = c
        .ok(
            &admin,
            "POST",
            &format!("{}/preview", existing(mapping)),
            Some(json!({"delete":true})),
        )
        .await;
    assert!(preview["immediate"]["applies"].as_bool().unwrap());
    assert!(
        !preview["immediate"]["workspace_membership_revoked"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let digest = preview["digest"].as_str().unwrap().to_owned();
    assert_eq!(digest.len(), 64);
    for (m, body) in [
        (m_pl, json!({"delete":true})),
        (m_pl, json!({"platform_role":"admin"})),
        (m_wsa, json!({"workspace_role":"admin"})),
        (m_wsa, json!({"workspace_id":ws2})),
        (m_wsb, json!({"workspace_role":"member"})),
        (m_wsa, json!({"enabled":true})),
        (m_wsa, json!({"delete":true})),
    ] {
        let value = c
            .ok(
                &admin,
                "POST",
                &format!("{}/preview", existing(m)),
                Some(body),
            )
            .await;
        assert_eq!(value["digest"].as_str().unwrap().len(), 64);
    }
    c.send(
        &auditor,
        "POST",
        new_preview,
        Some(mapping_body),
        StatusCode::FORBIDDEN,
    )
    .await;
    c.ok(
        &owner,
        "DELETE",
        &format!("{own}/members/{}", f.member.user_id),
        None,
    )
    .await;
    let members = c.ok(&owner, "GET", &format!("{own}/members"), None).await;
    assert_eq!(
        members["data"]
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["user_id"] == json!(f.member.user_id))
            .unwrap()["membership_source"],
        "group"
    );
    c.ok(
        &admin,
        "POST",
        &format!("{platform}/members"),
        Some(json!({"user_id":f.member.user_id,"role":"member"})),
    )
    .await;
    c.ok(
        &admin,
        "DELETE",
        &format!("{platform}/members/{}", f.member.user_id),
        None,
    )
    .await;
    c.send(
        &owner,
        "DELETE",
        &format!("{own}/members/{}", f.owner.user_id),
        None,
        StatusCode::CONFLICT,
    )
    .await;
    // A stale digest is refused; the preview's own digest is a positive precondition.
    let stale = format!("\"{}\"", "0".repeat(64));
    c.send_with(
        &admin,
        "PATCH",
        &mapping_path,
        Some(json!({"enabled":false})),
        StatusCode::PRECONDITION_FAILED,
        &[("if-match", stale.as_str())],
    )
    .await;
    let current = format!("\"{digest}\"");
    c.send_with(
        &admin,
        "PATCH",
        &mapping_path,
        Some(json!({"enabled":false})),
        StatusCode::OK,
        &[("if-match", current.as_str())],
    )
    .await;
    // The delete precondition follows the stored state: preview, then delete with its digest.
    let digest = c
        .ok(
            &admin,
            "POST",
            &format!("{mapping_path}/preview"),
            Some(json!({"enabled":false})),
        )
        .await["digest"]
        .as_str()
        .unwrap()
        .to_owned();
    let current = format!("\"{digest}\"");
    c.send_with(
        &admin,
        "DELETE",
        &mapping_path,
        None,
        StatusCode::OK,
        &[("if-match", current.as_str())],
    )
    .await;

    // One-time values are asserted in memory only; list representation has no token/hash.
    let invite = c
        .ok(
            &owner,
            "POST",
            &format!("{own}/invitations"),
            Some(json!({"email":f.outsider.email,"role":"member"})),
        )
        .await;
    assert_eq!(invite["token"].as_str().unwrap().len(), 64);
    let revoked = c
        .ok(
            &owner,
            "POST",
            &format!("{own}/invitations"),
            Some(json!({"email":"invitee-revoked@example.test","role":"admin"})),
        )
        .await;
    c.ok(
        &owner,
        "DELETE",
        &format!("{own}/invitations/{}", id(&revoked)),
        None,
    )
    .await;
    let accepted = c
        .ok(
            &owner,
            "POST",
            &format!("{own}/invitations"),
            Some(json!({"email":"invitee-accepted@example.test","role":"member"})),
        )
        .await;
    // What accepting an invitation records (members.rs accept path).
    sqlx::query("UPDATE workspace_invitations SET accepted_at=now() WHERE id=$1")
        .bind(id(&accepted))
        .execute(&pool)
        .await
        .unwrap();
    let invites = c
        .ok(&owner, "GET", &format!("{own}/invitations"), None)
        .await;
    assert!(!invites["data"].as_array().unwrap().is_empty());
    assert!(invites["data"][0].get("token").is_none());
    assert!(invites["data"][0].get("token_hash").is_none());
    c.ok(
        &owner,
        "DELETE",
        &format!("{own}/invitations/{}", id(&invite)),
        None,
    )
    .await;

    // Access requests: both queues and all four decision operations, with and
    // without a reason and a profile name, including current-eligibility
    // redaction and the requester-private decision projection.
    c.ok(
        &admin,
        "PUT",
        &format!("{platform}/discoverable"),
        Some(json!({"discoverable":true})),
    )
    .await;
    let mut requesters = Vec::new();
    for (i, (scope, outcome)) in [
        ("platform", "approve"),
        ("platform", "deny"),
        ("workspace", "approve"),
        ("workspace", "deny"),
    ]
    .into_iter()
    .enumerate()
    {
        for named in [true, false] {
            let n = i * 2 + usize::from(named);
            let requester = user(&pool, &format!("requester-{n}"), "user").await;
            if named {
                profile(&pool, requester.user_id, Some("Requester Named"), false).await;
            }
            let request_browser = browser(&pool, &requester).await;
            let reason = named.then_some("Contract request");
            let request=id(&c.ok(&request_browser,"POST","/api/v1/me/access-requests",Some(json!({"workspace_id":ws,"reason":reason,"idempotency_key":format!("slice2-{n}")}))).await);
            for (who, path) in [
                (&auditor, "/api/v1/platform/access-requests".to_owned()),
                (&owner, format!("{own}/access-requests")),
            ] {
                let queue = c.ok(who, "GET", &path, None).await;
                assert!(
                    queue["data"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|v| v["requester_email"] == requester.email)
                );
            }
            let decision = if scope == "platform" {
                format!("/api/v1/platform/access-requests/{request}/{outcome}")
            } else {
                format!("{own}/access-requests/{request}/{outcome}")
            };
            c.send(
                &auditor,
                "POST",
                &format!("/api/v1/platform/access-requests/{request}/{outcome}"),
                None,
                StatusCode::FORBIDDEN,
            )
            .await;
            c.send(
                &owner,
                "POST",
                &format!(
                    "/api/v1/workspaces/{}/access-requests/{request}/{outcome}",
                    f.project
                ),
                None,
                StatusCode::NOT_FOUND,
            )
            .await;
            let value = c
                .ok(
                    if scope == "platform" { &admin } else { &owner },
                    "POST",
                    &decision,
                    None,
                )
                .await;
            assert_eq!(
                value["status"],
                if outcome == "approve" {
                    "approved"
                } else {
                    "denied"
                }
            );
            assert!(value.get("requester_email").is_none());
            requesters.push((requester.clone(), request));
            if i == 3 && named {
                c.ok(
                    &admin,
                    "PATCH",
                    &format!("/api/v1/platform/users/{}", requester.user_id),
                    Some(json!({"disabled":true})),
                )
                .await;
                for (who, path) in [
                    (&auditor, "/api/v1/platform/access-requests".to_owned()),
                    (&owner, format!("{own}/access-requests")),
                ] {
                    let value = c.ok(who, "GET", &path, None).await;
                    let redacted = value["data"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .find(|v| v["id"] == json!(request))
                        .unwrap();
                    assert!(redacted["requester_email"].is_null());
                    assert!(redacted["requester_display_name"].is_null());
                }
            }
        }
    }
    // One person with a decided request (with a reason) and a pending one (without).
    c.ok(
        &admin,
        "PUT",
        &format!("/api/v1/platform/workspaces/{ws2}/discoverable"),
        Some(json!({"discoverable":true})),
    )
    .await;
    let decided_requester = requesters[0].0.clone();
    let decided_browser = browser(&pool, &decided_requester).await;
    c.ok(
        &decided_browser,
        "POST",
        "/api/v1/me/access-requests",
        Some(json!({"workspace_id":ws2,"idempotency_key":"slice2-pending"})),
    )
    .await;

    // Catalog and policy layers: populated replacement/default lists, never a union.
    let catalog = Uuid::new_v4();
    sqlx::query("INSERT INTO catalogs(id,name) VALUES($1,'Contract catalog')")
        .bind(catalog)
        .execute(&pool)
        .await
        .unwrap();
    let m = model(&pool, "contract-access-model").await;
    let access = format!("{own}/access");
    // A route, and a key in the workspace, for the attempts seeded below.
    let connection = Uuid::new_v4();
    let deployment = Uuid::new_v4();
    sqlx::query("INSERT INTO provider_connections(id,name,provider,credential_ref) VALUES($1,'Contract mock','openai','env:UNUSED')").bind(connection).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO deployments(id,model_id,provider_connection_id,upstream_model) VALUES($1,$2,$3,'mock')").bind(deployment).bind(m).bind(connection).execute(&pool).await.unwrap();
    let (status, ws_key) = key(&f, &f.owner, ws, Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    // Every layer unset first: the project type default carries no limit, no
    // override exists and the workspace has no local policy.
    let nulls = json!({"requests_per_minute":null,"tokens_per_minute":null,"concurrent_requests":null,"concurrent_jobs":null,"storage_bytes":null,"key_issuances_per_holder":null,"key_issuances_per_workspace":null,"budgets":[]});
    c.ok(
        &admin,
        "PUT",
        "/api/v1/platform/workspace-types/project/policy",
        Some(nulls),
    )
    .await;
    c.ok(&owner, "GET", &access, None).await;
    assert!(
        c.ok(
            &auditor,
            "GET",
            "/api/v1/platform/workspace-types/project/policy",
            None
        )
        .await["policy"]["storage_bytes"]
            .is_null()
    );
    c.ok(
        &auditor,
        "GET",
        "/api/v1/platform/workspace-types/team/policy",
        None,
    )
    .await;
    let bare = c
        .ok(&auditor, "GET", &format!("{platform}/policy"), None)
        .await;
    assert!(bare["effective"]["requests_per_minute"].is_null());
    assert!(bare["storage"]["quota_bytes"].is_null());
    c.ok(
        &admin,
        "PUT",
        "/api/v1/platform/workspace-types/project/catalogs",
        Some(json!({"catalog_ids":[catalog]})),
    )
    .await;
    assert!(
        !c.ok(
            &auditor,
            "GET",
            "/api/v1/platform/workspace-types/project/catalogs",
            None
        )
        .await["catalog_ids"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    // A workspace override that sets nothing: present, with every limit empty.
    c.ok(
        &admin,
        "PUT",
        &format!("{platform}/policy"),
        Some(json!({"requests_per_minute":null,"tokens_per_minute":null,"concurrent_requests":null,"concurrent_jobs":null,"storage_bytes":null,"key_issuances_per_holder":null,"key_issuances_per_workspace":null,"monthly_budget_microusd":null})),
    )
    .await;
    c.ok(&owner, "GET", &access, None).await;
    let limits = json!({"requests_per_minute":100,"tokens_per_minute":1000,"concurrent_requests":4,"concurrent_jobs":2,"storage_bytes":4096,"key_issuances_per_holder":10,"key_issuances_per_workspace":100,"budgets":[{"period":"month","amount_microusd":"9007199254740993"}]});
    c.ok(
        &admin,
        "PUT",
        "/api/v1/platform/workspace-types/project/policy",
        Some(limits.clone()),
    )
    .await;
    let policy = c
        .ok(
            &auditor,
            "GET",
            "/api/v1/platform/workspace-types/project/policy",
            None,
        )
        .await;
    assert_eq!(
        policy["policy"]["budgets"][0]["amount_microusd"],
        "9007199254740993"
    );
    // Type default only: the override layer is absent.
    c.ok(&owner, "GET", &access, None).await;
    // A partial override (only requests_per_minute), then the access view with it.
    c.ok(
        &admin,
        "PUT",
        &format!("{platform}/catalogs"),
        Some(json!({"mode":"replace","catalog_ids":[catalog]})),
    )
    .await;
    assert!(
        !c.ok(&auditor, "GET", &format!("{platform}/catalogs"), None)
            .await["effective_catalog_ids"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    c.ok(
        &admin,
        "PUT",
        &format!("{platform}/policy"),
        Some(json!({"requests_per_minute":50,"tokens_per_minute":null,"concurrent_requests":null,"monthly_budget_microusd":null})),
    )
    .await;
    c.ok(&owner, "GET", &access, None).await;
    // A workspace-local policy (set by the owner through the workspace route).
    c.setup(
        &owner,
        "PUT",
        &format!("{own}/policy"),
        Some(json!({"requests_per_minute":10,"tokens_per_minute":500,"concurrent_requests":2,"concurrent_jobs":1,"storage_bytes":2048,"key_issuances_per_holder":5,"key_issuances_per_workspace":50,"budgets":[{"period":"month","amount_microusd":"5000000"},{"period":"lifetime","amount_microusd":"9000000"}]})),
    )
    .await;
    // The full override replaces the partial one.
    c.ok(
        &admin,
        "PUT",
        &format!("{platform}/policy"),
        Some(limits.clone()),
    )
    .await;
    assert!(
        !c.ok(&auditor, "GET", &format!("{platform}/policy"), None)
            .await["budgets"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    // An attempt of unknown cost: no window can state its remainder any more.
    unknown_cost_attempt(&pool, ws, id(&ws_key), deployment).await;
    let unknown_windows = c
        .ok(&auditor, "GET", &format!("{platform}/policy"), None)
        .await;
    assert!(unknown_windows["budgets"][0]["remaining_microusd"].is_null());
    assert!(
        unknown_windows["budgets"][0]["unknown_cost_requests"] != "0",
        "unknown-cost window counts the attempt"
    );
    assert!(unknown_windows["budgets"][0]["used_microusd"].is_string());
    // A window whose accounting row is unreadable: the drift seeded exactly as
    // governance/tests.rs does it (negative counters), on a dedicated workspace
    // and restored afterwards.
    let drift_ws = id(&c
        .ok(
            &admin,
            "POST",
            "/api/v1/platform/workspaces",
            Some(json!({"name":"Contract drift","kind":"project","owner_user_id":f.owner.user_id})),
        )
        .await);
    let drift_policy = format!("/api/v1/platform/workspaces/{drift_ws}/policy");
    c.ok(
        &admin,
        "PUT",
        &drift_policy,
        Some(policy_body(
            json!({"budgets":[{"period":"day","amount_microusd":"100"},{"period":"month","amount_microusd":"1000"}]}),
        )),
    )
    .await;
    let (status, drift_key) = key(&f, &f.owner, drift_ws, Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    attempt(
        &pool,
        drift_ws,
        id(&drift_key),
        deployment,
        "contract-drift",
        None,
        true,
    )
    .await;
    let healthy = c.ok(&auditor, "GET", &drift_policy, None).await;
    assert!(
        healthy["budgets"][0]["period"] == "day"
            && healthy["budgets"][0]["headroom_status"] == "known"
            && healthy["budgets"][0]["settled_microusd"] == "7",
        "drift workspace day window starts known"
    );
    let saved: (String, String) = sqlx::query_as("SELECT settled_microusd::text,held_microusd::text FROM budget_totals WHERE scope_kind='workspace' AND scope_id=$1 AND period='day'")
        .bind(drift_ws)
        .fetch_one(&pool)
        .await
        .unwrap();
    let corrupted = sqlx::query("UPDATE budget_totals SET settled_microusd=-1,held_microusd=0 WHERE scope_kind='workspace' AND scope_id=$1 AND period='day'")
        .bind(drift_ws)
        .execute(&pool)
        .await
        .unwrap()
        .rows_affected();
    assert_eq!(corrupted, 1);
    let drifted = c.ok(&auditor, "GET", &drift_policy, None).await;
    assert!(
        drifted["budgets"][0]["headroom_status"] == "unknown",
        "drifted window is unknown"
    );
    for field in [
        "used_microusd",
        "unresolved_usage",
        "exhausted",
        "settled_microusd",
        "pending_held_microusd",
        "unknown_held_microusd",
        "unknown_cost_requests",
        "remaining_microusd",
    ] {
        assert!(drifted["budgets"][0][field].is_null());
    }
    // The neighbouring window stays readable.
    assert!(
        drifted["budgets"][1]["headroom_status"] == "known",
        "neighbouring window stays known"
    );
    sqlx::query("UPDATE budget_totals SET settled_microusd=$2::numeric,held_microusd=$3::numeric WHERE scope_kind='workspace' AND scope_id=$1 AND period='day'")
        .bind(drift_ws)
        .bind(&saved.0)
        .bind(&saved.1)
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        c.ok(&auditor, "GET", &drift_policy, None).await["budgets"][0]["headroom_status"]
            == "known",
        "restored window is known again"
    );
    c.ok(
        &admin,
        "POST",
        &format!("{platform}/models"),
        Some(json!({"model_id":m})),
    )
    .await;
    let full = c.ok(&owner, "GET", &access, None).await;
    assert!(!full["models"].as_array().unwrap().is_empty());
    c.ok(
        &owner,
        "GET",
        &target(&access, &format!("model_id={m}")),
        None,
    )
    .await;
    c.ok(&admin, "DELETE", &format!("{platform}/policy"), None)
        .await;
    c.ok(&admin, "DELETE", &format!("{platform}/catalogs"), None)
        .await;
    c.ok(&admin, "DELETE", &format!("{platform}/models/{m}"), None)
        .await;

    // Nonempty inference session reads: unknown usage and settled usage,
    // including own-vs-wide refusal.
    let (status, key_value) = key(&f, &f.owner, f.team, Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    let key_id = id(&key_value);
    let unknown = attempt(
        &pool,
        f.team,
        key_id,
        deployment,
        "contract-session",
        None,
        false,
    )
    .await;
    // A newer session with tokens, a settled cost and an application label.
    attempt(
        &pool,
        f.team,
        key_id,
        deployment,
        "contract-session-b",
        Some("Contract Agent"),
        true,
    )
    .await;
    let session_path = format!("/api/v1/workspaces/{}/sessions", f.team);
    let page = c
        .ok(&owner, "GET", &target(&session_path, "limit=1"), None)
        .await;
    assert_eq!(page["data"].as_array().unwrap().len(), 1);
    let cursor = page["next_cursor"].as_str().unwrap().to_owned();
    let all = c.ok(&owner, "GET", &session_path, None).await;
    assert_eq!(all["data"].as_array().unwrap().len(), 2);
    let session = c
        .ok(
            &owner,
            "GET",
            &format!("{session_path}/contract-session"),
            None,
        )
        .await;
    assert!(session["cost_microusd"].is_null());
    assert!(session["input_tokens"].is_null());
    let session_b = c
        .ok(
            &owner,
            "GET",
            &format!("{session_path}/contract-session-b"),
            None,
        )
        .await;
    assert_eq!(session_b["cost_microusd"], "7");
    assert_eq!(session_b["input_tokens"], "10");
    c.send(
        &regular,
        "GET",
        &format!("{session_path}/contract-session"),
        None,
        StatusCode::NOT_FOUND,
    )
    .await;
    c.send(
        &admin,
        "GET",
        &format!("{session_path}/contract-session"),
        None,
        StatusCode::FORBIDDEN,
    )
    .await;
    c.send(
        &admin,
        "GET",
        &format!("/api/v1/workspaces/{foreign}/sessions"),
        None,
        StatusCode::NOT_FOUND,
    )
    .await;
    // Every filter at once, valid for the older session (the cursor is the newer page's).
    let today = chrono::Utc::now().date_naive();
    let start = (today - chrono::TimeDelta::days(1)).to_string();
    let end = (today + chrono::TimeDelta::days(1)).to_string();
    let prefix = unknown.to_string()[..8].to_owned();
    let key_text = key_id.to_string();
    let filters: Vec<(&str, String)> = vec![
        ("start_date", start),
        ("end_date", end),
        ("model", "contract-access-model".into()),
        ("status", "succeeded".into()),
        ("q", prefix),
        ("finish_reason", "stop".into()),
        ("workload", "generation".into()),
        ("key_id", key_text),
        ("streamed", "false".into()),
    ];
    let list = c
        .ok(
            &owner,
            "GET",
            &target(
                &session_path,
                &render(
                    &filters,
                    &[
                        ("session_id", "contract-session"),
                        ("cursor", cursor.as_str()),
                        ("limit", "1"),
                    ],
                ),
            ),
            None,
        )
        .await;
    assert_eq!(list["data"].as_array().unwrap().len(), 1);
    let detail_path = format!("{session_path}/contract-session");
    let detail = c
        .ok(
            &owner,
            "GET",
            &target(&detail_path, &render(&filters, &[])),
            None,
        )
        .await;
    assert!(
        detail["session_id"] == "contract-session",
        "the detail answers a different session"
    );

    // Platform subject exports are distinct, closed privacy projections.
    let export = c
        .ok(
            &auditor,
            "GET",
            &format!("/api/v1/platform/users/{}/export", f.owner.user_id),
            None,
        )
        .await;
    assert!(
        !export["shared_memberships"]["items"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(!export["audit"]["items"].as_array().unwrap().is_empty());
    for private in ["memberships", "api_keys", "sessions", "requests", "files"] {
        assert!(export.get(private).is_none());
    }
    assert!(!export.to_string().contains(&f.personal.to_string()));
    // A cleaned-up person (cleaned, null address, system audit), an Admin whose
    // audit rows have no resource, a group-role holder with a name and sign-ins,
    // and a person with a decided request and a pending one. Exports are limited
    // to three per hour and actor, so they are split across both platform readers.
    for (reader, who) in [
        (&auditor, new_user),
        (&auditor, b),
        (&admin, f.admin.user_id),
        (&admin, decided_requester.user_id),
    ] {
        c.ok(
            reader,
            "GET",
            &format!("/api/v1/platform/users/{who}/export"),
            None,
        )
        .await;
    }

    // Directory reads over the seeded identities.
    let users = "/api/v1/platform/users";
    assert!(
        !c.ok(&auditor, "GET", users, None).await["data"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    c.ok(&auditor, "GET", &target(users, "status=suspended"), None)
        .await;
    c.ok(&auditor, "GET", &target(users, "role=none"), None)
        .await;
    c.ok(
        &auditor,
        "GET",
        &target(users, "limit=50&offset=0&q=group&role=any&status=active"),
        None,
    )
    .await;
    for who in [b, new_user, cc.user_id, f.member.user_id] {
        c.ok(
            &auditor,
            "GET",
            &format!("/api/v1/platform/users/{who}"),
            None,
        )
        .await;
    }
    c.ok(
        &auditor,
        "GET",
        &target(
            "/api/v1/platform/workspaces",
            "limit=50&offset=0&q=contract&kind=project&status=active",
        ),
        None,
    )
    .await;
    c.ok(
        &auditor,
        "GET",
        &target("/api/v1/platform/workspaces", "status=disabled"),
        None,
    )
    .await;
    c.ok(
        &auditor,
        "GET",
        &format!("/api/v1/platform/workspaces/{wsd}"),
        None,
    )
    .await;
    c.ok(&auditor, "GET", &platform, None).await;
    for (who, path) in [
        (&owner, format!("{own}/members")),
        (&auditor, format!("{platform}/members")),
    ] {
        c.ok(
            who,
            "GET",
            &target(&path, "limit=50&offset=0&q=group"),
            None,
        )
        .await;
    }
    for (who, path) in [
        (&auditor, "/api/v1/platform/access-requests".to_owned()),
        (&owner, format!("{own}/access-requests")),
        (&owner, format!("{own}/invitations")),
        (&auditor, "/api/v1/platform/oidc/group-mappings".to_owned()),
    ] {
        c.ok(who, "GET", &target(&path, "limit=50&offset=0"), None)
            .await;
    }

    // HTTP boundary vectors: query parameters, then request bodies. A failing set
    // is recorded and reported with the rest, never printed with a value.
    let page_base = || pairs(&[("limit", "50"), ("offset", "0")]);
    {
        let mut o = c.observed.lock().await;
        c.guarded(
            "GET /platform/users",
            query_vectors_fixed(
                &mut o,
                "GET",
                "/api/v1/platform/users",
                &[
                    ("limit", "50"),
                    ("offset", "0"),
                    ("q", "group"),
                    ("role", "any"),
                    ("status", "active"),
                ],
                async |q| {
                    c.raw(
                        &auditor,
                        "GET",
                        &target("/api/v1/platform/users", &q),
                        None,
                        &[],
                    )
                    .await
                },
            ),
        )
        .await;
        c.guarded(
            "GET /platform/workspaces",
            query_vectors_fixed(
                &mut o,
                "GET",
                "/api/v1/platform/workspaces",
                &[
                    ("limit", "50"),
                    ("offset", "0"),
                    ("q", "contract"),
                    ("kind", "project"),
                    ("status", "active"),
                ],
                async |q| {
                    c.raw(
                        &auditor,
                        "GET",
                        &target("/api/v1/platform/workspaces", &q),
                        None,
                        &[],
                    )
                    .await
                },
            ),
        )
        .await;
        for (label, template, who, path) in [
            (
                "GET /platform/workspaces/{ws}/members",
                "/api/v1/platform/workspaces/{ws}/members",
                &auditor,
                format!("{platform}/members"),
            ),
            (
                "GET /workspaces/{ws}/members",
                "/api/v1/workspaces/{ws}/members",
                &owner,
                format!("{own}/members"),
            ),
        ] {
            c.guarded(
                label,
                query_vectors_fixed(
                    &mut o,
                    "GET",
                    template,
                    &[("limit", "50"), ("offset", "0"), ("q", "group")],
                    async |q| c.raw(who, "GET", &target(&path, &q), None, &[]).await,
                ),
            )
            .await;
        }
        for (label, template, who, path) in [
            (
                "GET /platform/workspaces/{ws}/member-candidates",
                "/api/v1/platform/workspaces/{ws}/member-candidates",
                &auditor,
                format!("{platform}/member-candidates"),
            ),
            (
                "GET /workspaces/{ws}/member-candidates",
                "/api/v1/workspaces/{ws}/member-candidates",
                &owner,
                format!("{own}/member-candidates"),
            ),
        ] {
            c.guarded(
                label,
                query_vectors_fixed(&mut o, "GET", template, &[("q", "member")], async |q| {
                    c.raw(who, "GET", &target(&path, &q), None, &[]).await
                }),
            )
            .await;
        }
        for (label, template, who, path) in [
            (
                "GET /platform/access-requests",
                "/api/v1/platform/access-requests",
                &auditor,
                "/api/v1/platform/access-requests".to_owned(),
            ),
            (
                "GET /workspaces/{ws}/access-requests",
                "/api/v1/workspaces/{ws}/access-requests",
                &owner,
                format!("{own}/access-requests"),
            ),
            (
                "GET /workspaces/{ws}/invitations",
                "/api/v1/workspaces/{ws}/invitations",
                &owner,
                format!("{own}/invitations"),
            ),
            (
                "GET /platform/oidc/group-mappings",
                "/api/v1/platform/oidc/group-mappings",
                &auditor,
                "/api/v1/platform/oidc/group-mappings".to_owned(),
            ),
        ] {
            c.guarded(
                label,
                query_vectors(
                    &mut o,
                    "GET",
                    template,
                    &[],
                    async || ((), page_base()),
                    async |_: &(), q| {
                        Probe::from(c.raw(who, "GET", &target(&path, &q), None, &[]).await)
                    },
                ),
            )
            .await;
        }
        let model_text = m.to_string();
        c.guarded(
            "GET /workspaces/{ws}/access",
            query_vectors_fixed(
                &mut o,
                "GET",
                "/api/v1/workspaces/{ws}/access",
                &[("model_id", model_text.as_str())],
                async |q| c.raw(&owner, "GET", &target(&access, &q), None, &[]).await,
            ),
        )
        .await;
        let mut base: Vec<(&str, String)> = filters.clone();
        base.extend([
            ("session_id", "contract-session".to_owned()),
            ("cursor", cursor.clone()),
            ("limit", "1".to_owned()),
        ]);
        // Boundary candidates for the query patterns whose length the harness cannot
        // derive: each pair straddles the boundary within one edit.
        let cut = |text: &str| text.chars().skip(1).collect::<String>();
        let session_candidates: Vec<(&str, String)> = vec![
            ("status", "succeeded".to_owned()),
            ("status", cut("succeeded")),
            ("finish_reason", "stop".to_owned()),
            ("finish_reason", cut("stop")),
            ("cursor", cursor.clone()),
            ("cursor", cursor.replacen('_', "", 1)),
            ("q", "abcd".to_owned()),
            ("q", "abc".to_owned()),
            ("q", "a".repeat(36)),
            ("q", "a".repeat(37)),
        ];
        let detail_candidates: Vec<(&str, &str)> = session_candidates
            .iter()
            .filter(|(name, _)| *name != "cursor")
            .map(|(name, value)| (*name, value.as_str()))
            .collect();
        let session_candidates: Vec<(&str, &str)> = session_candidates
            .iter()
            .map(|(name, value)| (*name, value.as_str()))
            .collect();
        let list_base: Vec<(String, String)> = base
            .iter()
            .map(|(n, v)| ((*n).to_owned(), v.clone()))
            .collect();
        c.guarded(
            "GET /workspaces/{ws}/sessions",
            query_vectors_extended(
                &mut o,
                "GET",
                "/api/v1/workspaces/{ws}/sessions",
                &[],
                &session_candidates,
                async || ((), list_base.clone()),
                async |_: &(), q| {
                    Probe::from(
                        c.raw(&owner, "GET", &target(&session_path, &q), None, &[])
                            .await,
                    )
                },
            ),
        )
        .await;
        let detail_base: Vec<(String, String)> = filters
            .iter()
            .map(|(n, v)| ((*n).to_owned(), v.clone()))
            .collect();
        c.guarded(
            "GET /workspaces/{ws}/sessions/{session}",
            query_vectors_extended(
                &mut o,
                "GET",
                "/api/v1/workspaces/{ws}/sessions/{session}",
                &[],
                &detail_candidates,
                async || ((), detail_base.clone()),
                async |_: &(), q| {
                    Probe::from(
                        c.raw(&owner, "GET", &target(&detail_path, &q), None, &[])
                            .await,
                    )
                },
            ),
        )
        .await;
    }

    // Request bodies. Targets are fresh so each vector set starts from known state.
    let vector_user = user(&pool, "vector-user", "user").await;
    let vector_role_user = user(&pool, "vector-role", "user").await;
    let vector_member = user(&pool, "vector-member", "user").await;
    let vector_member2 = user(&pool, "vector-member2", "user").await;
    let vector_ws = id(&c
        .ok(
            &admin,
            "POST",
            "/api/v1/platform/workspaces",
            Some(json!({"name":"Vector shared","kind":"project","owner_user_id":f.owner.user_id})),
        )
        .await);
    let vector_mapping = id(&c
        .ok(
            &admin,
            "POST",
            "/api/v1/platform/oidc/group-mappings",
            Some(json!({"issuer":ISSUER,"group_value":"vector-group","target_kind":"platform","platform_role":"user","workspace_id":null,"workspace_role":null,"enabled":true})),
        )
        .await);
    let vector_path = format!("/api/v1/workspaces/{vector_ws}");
    let vector_platform = format!("/api/v1/platform/workspaces/{vector_ws}");
    // The platform workspace PATCH vectors flip `disabled` to true (a real 200), so
    // they get their own workspace: disabling the shared vector workspace would turn
    // every later vector against it into a 404 that proves nothing.
    let patch_ws = id(&c
        .ok(
            &admin,
            "POST",
            "/api/v1/platform/workspaces",
            Some(json!({"name":"Vector patch target","kind":"project","owner_user_id":f.owner.user_id})),
        )
        .await);
    let patch_platform = format!("/api/v1/platform/workspaces/{patch_ws}");
    macro_rules! body {
        ($label:expr, $method:expr, $template:expr, $who:expr, $path:expr, $valid:expr) => {{
            let path: String = $path;
            let mut o = c.observed.lock().await;
            c.guarded(
                $label,
                body_vectors_http(&mut o, $method, $template, $valid, async |body| {
                    Probe::from(c.raw($who, $method, &path, Some(body), &[]).await)
                }),
            )
            .await;
        }};
    }
    body!(
        "POST /platform/users",
        "POST",
        "/api/v1/platform/users",
        &admin,
        "/api/v1/platform/users".to_owned(),
        json!({"email":"vector-created@example.test","platform_role":"user"})
    );
    body!(
        "PATCH /platform/users/{user}",
        "PATCH",
        "/api/v1/platform/users/{user}",
        &admin,
        format!("/api/v1/platform/users/{}", vector_user.user_id),
        json!({"email":"vector-patched@example.test","disabled":false,"oidc_link_allowed":true})
    );
    // The null side of each optional field (null is absence for the handler).
    body!(
        "PATCH /platform/users/{user} (nulls)",
        "PATCH",
        "/api/v1/platform/users/{user}",
        &admin,
        format!("/api/v1/platform/users/{}", vector_user.user_id),
        json!({"email":"vector-patched-nulls@example.test","disabled":null,"oidc_link_allowed":null})
    );
    body!(
        "PATCH /platform/users/{user} (positional, nulls)",
        "PATCH",
        "/api/v1/platform/users/{user}",
        &admin,
        format!("/api/v1/platform/users/{}", vector_user.user_id),
        json!(["vector-patched-nulls-p@example.test", null, null])
    );
    body!(
        "POST /platform/users/{user}/roles",
        "POST",
        "/api/v1/platform/users/{user}/roles",
        &admin,
        format!("/api/v1/platform/users/{}/roles", vector_role_user.user_id),
        json!({"role":"user"})
    );
    body!(
        "POST /platform/workspaces",
        "POST",
        "/api/v1/platform/workspaces",
        &admin,
        "/api/v1/platform/workspaces".to_owned(),
        json!({"name":"Vector created","kind":"project","owner_user_id":f.owner.user_id})
    );
    body!(
        "PATCH /platform/workspaces/{ws}",
        "PATCH",
        "/api/v1/platform/workspaces/{ws}",
        &admin,
        patch_platform.clone(),
        json!({"name":"Vector renamed","disabled":false,"cost_center_id":cost_center})
    );
    body!(
        "PATCH /platform/workspaces/{ws} (nulls)",
        "PATCH",
        "/api/v1/platform/workspaces/{ws}",
        &admin,
        patch_platform.clone(),
        json!({"name":"Vector renamed nulls","disabled":null,"cost_center_id":null})
    );
    body!(
        "PATCH /platform/workspaces/{ws} (positional, nulls)",
        "PATCH",
        "/api/v1/platform/workspaces/{ws}",
        &admin,
        patch_platform.clone(),
        json!(["Vector renamed nulls positional", null, null])
    );
    body!(
        "PATCH /workspaces/{ws}",
        "PATCH",
        "/api/v1/workspaces/{ws}",
        &owner,
        vector_path.clone(),
        json!({"name":"Vector own","object_visibility":"workspace"})
    );
    body!(
        "PUT /platform/workspaces/{ws}/discoverable",
        "PUT",
        "/api/v1/platform/workspaces/{ws}/discoverable",
        &admin,
        format!("{vector_platform}/discoverable"),
        json!({"discoverable":true})
    );
    body!(
        "POST /platform/workspaces/{ws}/members",
        "POST",
        "/api/v1/platform/workspaces/{ws}/members",
        &admin,
        format!("{vector_platform}/members"),
        json!({"user_id":vector_member.user_id,"role":"member"})
    );
    body!(
        "POST /workspaces/{ws}/members",
        "POST",
        "/api/v1/workspaces/{ws}/members",
        &owner,
        format!("{vector_path}/members"),
        json!({"user_id":vector_member2.user_id,"role":"member"})
    );
    body!(
        "POST /workspaces/{ws}/invitations",
        "POST",
        "/api/v1/workspaces/{ws}/invitations",
        &owner,
        format!("{vector_path}/invitations"),
        json!({"email":"vector-invite@example.test","role":"member"})
    );
    body!(
        "POST /platform/workspaces/{ws}/models",
        "POST",
        "/api/v1/platform/workspaces/{ws}/models",
        &admin,
        format!("{vector_platform}/models"),
        json!({"model_id":m})
    );
    body!(
        "PUT /platform/workspaces/{ws}/catalogs",
        "PUT",
        "/api/v1/platform/workspaces/{ws}/catalogs",
        &admin,
        format!("{vector_platform}/catalogs"),
        json!({"mode":"replace","catalog_ids":[catalog]})
    );
    body!(
        "PUT /platform/workspace-types/{kind}/catalogs",
        "PUT",
        "/api/v1/platform/workspace-types/{kind}/catalogs",
        &admin,
        "/api/v1/platform/workspace-types/team/catalogs".to_owned(),
        json!({"catalog_ids":[catalog]})
    );
    for (label, template, path) in [
        (
            "PUT /platform/workspace-types/{kind}/policy (budgets)",
            "/api/v1/platform/workspace-types/{kind}/policy",
            "/api/v1/platform/workspace-types/team/policy".to_owned(),
        ),
        (
            "PUT /platform/workspaces/{ws}/policy (budgets)",
            "/api/v1/platform/workspaces/{ws}/policy",
            format!("{vector_platform}/policy"),
        ),
    ] {
        // Every rate non-null, so each bound runs. ONE budget per run: a repeated
        // period is refused by the handler (policies.rs:569-581) and not
        // expressible in the contract, so a second budget would turn every
        // period vector into a duplicate. The two forms of a budget item are
        // separate runs (the object, then the tuple).
        for (form, budgets) in [
            (
                "object",
                json!([{"period":"month","amount_microusd":"9007199254740993"}]),
            ),
            ("tuple", json!([["month", "9007199254740993"]])),
        ] {
            body!(
                &format!("{label} ({form} item)"),
                "PUT",
                template,
                &admin,
                path.clone(),
                json!({"requests_per_minute":100,"tokens_per_minute":100,"concurrent_requests":3,"concurrent_jobs":2,"storage_bytes":1024,"key_issuances_per_holder":3,"key_issuances_per_workspace":9,"budgets":budgets})
            );
        }
    }
    // The monthly (deprecated single-budget) form is refused with 409 while more
    // than one budget is stored (policies.rs:372-374), whatever the body says, so
    // each of its runs starts from a single stored budget.
    let reset_policy = async |path: &str| {
        c.setup(
            &admin,
            "PUT",
            path,
            Some(policy_body(
                json!({"budgets":[{"period":"month","amount_microusd":"5"}]}),
            )),
        )
        .await;
    };
    for (label, template, path) in [
        (
            "PUT /platform/workspace-types/{kind}/policy (monthly)",
            "/api/v1/platform/workspace-types/{kind}/policy",
            "/api/v1/platform/workspace-types/team/policy".to_owned(),
        ),
        (
            "PUT /platform/workspaces/{ws}/policy (monthly)",
            "/api/v1/platform/workspaces/{ws}/policy",
            format!("{vector_platform}/policy"),
        ),
    ] {
        reset_policy(&path).await;
        body!(
            label,
            "PUT",
            template,
            &admin,
            path.clone(),
            json!({"requests_per_minute":null,"tokens_per_minute":null,"concurrent_requests":null,"monthly_budget_microusd":"5000","budget_period":"month"})
        );
        // Every optional rate explicitly null (the null side of each union).
        reset_policy(&path).await;
        body!(
            &format!("{label} (nulls)"),
            "PUT",
            template,
            &admin,
            path,
            json!({"requests_per_minute":null,"tokens_per_minute":null,"concurrent_requests":null,"concurrent_jobs":null,"storage_bytes":null,"key_issuances_per_holder":null,"key_issuances_per_workspace":null,"monthly_budget_microusd":null,"budget_period":"month"})
        );
    }
    // Handler-derived policy vectors the derived set cannot generate: the
    // positive-i64 money bound and its documented leading-zero divergence, the
    // exclusivity of budgets and the monthly form, and repeated periods.
    for (target, path) in [
        (
            "type",
            "/api/v1/platform/workspace-types/team/policy".to_owned(),
        ),
        ("workspace", format!("{vector_platform}/policy")),
    ] {
        c.guarded(
            &format!("PUT policy probes ({target})"),
            async {
                reset_policy(&path).await;
                // (what, extra fields, schema accepts, handler accepts)
                let probes: Vec<(&str, Value, bool, bool)> = vec![
                    ("monthly money i64 max", json!({"monthly_budget_microusd":"9223372036854775807"}), true, true),
                    ("monthly money i64 max + 1", json!({"monthly_budget_microusd":"9223372036854775808"}), false, false),
                    ("monthly money nineteen nines", json!({"monthly_budget_microusd":"9999999999999999999"}), false, false),
                    ("monthly money zero", json!({"monthly_budget_microusd":"0"}), false, false),
                    // The handler reads "007" as 7; the contract documents canonical digits only.
                    ("monthly money leading zeros", json!({"monthly_budget_microusd":"007"}), false, true),
                    ("budgets money i64 max", json!({"budgets":[{"period":"month","amount_microusd":"9223372036854775807"}]}), true, true),
                    ("budgets money i64 max + 1", json!({"budgets":[{"period":"month","amount_microusd":"9223372036854775808"}]}), false, false),
                    ("budgets money nineteen nines", json!({"budgets":[{"period":"month","amount_microusd":"9999999999999999999"}]}), false, false),
                    ("budgets money zero", json!({"budgets":[{"period":"month","amount_microusd":"0"}]}), false, false),
                    ("budgets money leading zeros", json!({"budgets":[{"period":"month","amount_microusd":"007"}]}), false, true),
                    ("positional budget money i64 max + 1", json!({"budgets":[["month","9223372036854775808"]]}), false, false),
                    ("positional budget money i64 max", json!({"budgets":[["day","9223372036854775807"]]}), true, true),
                    // Exactly one of budgets / monthly_budget_microusd; budget_period only with the monthly form.
                    ("budgets and monthly", json!({"budgets":[],"monthly_budget_microusd":"5000"}), false, false),
                    ("budgets and budget_period", json!({"budgets":[],"budget_period":"month"}), false, false),
                    ("budgets and null monthly", json!({"budgets":[],"monthly_budget_microusd":null}), false, false),
                    ("budget_period alone", json!({"budget_period":"month"}), false, false),
                    ("neither budgets nor monthly", json!({}), false, false),
                    ("null monthly alone", json!({"monthly_budget_microusd":null}), true, true),
                    ("monthly with a period", json!({"monthly_budget_microusd":"5000","budget_period":"day"}), true, true),
                    ("monthly with a null period", json!({"monthly_budget_microusd":"5000","budget_period":null}), false, false),
                    // A repeated period is refused even with different amounts (parse_budgets,
                    // policies.rs:569-581); the contract cannot state it (no contains/maxContains
                    // in request schemas), so the schema accepts and the handler refuses.
                    ("repeated period, same amount", json!({"budgets":[{"period":"month","amount_microusd":"5"},{"period":"month","amount_microusd":"5"}]}), true, false),
                    ("repeated period, other amount", json!({"budgets":[{"period":"month","amount_microusd":"5"},{"period":"month","amount_microusd":"6"}]}), true, false),
                    ("repeated period, positional", json!({"budgets":[["day","5"],["day","6"]]}), true, false),
                    ("repeated period, mixed forms", json!({"budgets":[{"period":"week","amount_microusd":"5"},["week","6"]]}), true, false),
                    // Five budgets cannot have distinct periods (parse_budgets refuses more than four).
                    ("five budgets", json!({"budgets":[["day","5"],["week","5"],["month","5"],["lifetime","5"],["day","6"]]}), true, false),
                    ("two distinct periods", json!({"budgets":[["day","5"],{"period":"lifetime","amount_microusd":"6"}]}), true, true),
                ];
                for (what, extra, schema, handler) in probes {
                    c.pin_body(&admin, "PUT", &path, what, policy_body(extra), schema, handler)
                        .await;
                }
            },
        )
        .await;
    }
    for (kind, valid) in [
        (
            "platform",
            json!({"issuer":ISSUER,"group_value":"vector-created","target_kind":"platform","platform_role":"user","workspace_id":null,"workspace_role":null,"enabled":true}),
        ),
        (
            "workspace",
            json!({"issuer":ISSUER,"group_value":"vector-created","target_kind":"workspace","platform_role":null,"workspace_id":vector_ws,"workspace_role":"member","enabled":true}),
        ),
    ] {
        body!(
            &format!("POST /platform/oidc/group-mappings ({kind})"),
            "POST",
            "/api/v1/platform/oidc/group-mappings",
            &admin,
            "/api/v1/platform/oidc/group-mappings".to_owned(),
            valid.clone()
        );
        body!(
            &format!("POST /platform/oidc/group-mappings/preview ({kind})"),
            "POST",
            "/api/v1/platform/oidc/group-mappings/preview",
            &admin,
            "/api/v1/platform/oidc/group-mappings/preview".to_owned(),
            valid
        );
    }
    body!(
        "PATCH /platform/oidc/group-mappings/{id}",
        "PATCH",
        "/api/v1/platform/oidc/group-mappings/{id}",
        &admin,
        format!("/api/v1/platform/oidc/group-mappings/{vector_mapping}"),
        // Every mapping field, so each optional field's constraints run; the
        // merge is then fully determined by the body, whatever an accepted
        // vector stored before.
        json!({"issuer":ISSUER,"group_value":"vector-group","target_kind":"platform","platform_role":"user","workspace_id":null,"workspace_role":null,"enabled":true})
    );
    body!(
        "POST /platform/oidc/group-mappings/{id}/preview",
        "POST",
        "/api/v1/platform/oidc/group-mappings/{id}/preview",
        &admin,
        format!("/api/v1/platform/oidc/group-mappings/{vector_mapping}/preview"),
        json!({"issuer":ISSUER,"group_value":"vector-group","target_kind":"platform","platform_role":"user","workspace_id":null,"workspace_role":null,"enabled":true,"delete":false})
    );

    // The positional wire form (serde's struct-as-array representation) of every
    // DTO the handler deserializes, each with its own valid body: the arm of the
    // contract that the object-form runs above do not reach.
    let positional_policy = json!([100, 100, 3, 2, 1024, 3, 9, "5000", "month"]);
    body!(
        "POST /platform/users (positional)",
        "POST",
        "/api/v1/platform/users",
        &admin,
        "/api/v1/platform/users".to_owned(),
        json!(["vector-created-positional@example.test", "user"])
    );
    body!(
        "PATCH /platform/users/{user} (positional)",
        "PATCH",
        "/api/v1/platform/users/{user}",
        &admin,
        format!("/api/v1/platform/users/{}", vector_user.user_id),
        json!(["vector-patched-positional@example.test", false, true])
    );
    body!(
        "POST /platform/users/{user}/roles (positional)",
        "POST",
        "/api/v1/platform/users/{user}/roles",
        &admin,
        format!("/api/v1/platform/users/{}/roles", vector_role_user.user_id),
        json!(["user"])
    );
    body!(
        "POST /platform/workspaces (positional)",
        "POST",
        "/api/v1/platform/workspaces",
        &admin,
        "/api/v1/platform/workspaces".to_owned(),
        json!(["Vector created positional", "project", f.owner.user_id])
    );
    body!(
        "PATCH /platform/workspaces/{ws} (positional)",
        "PATCH",
        "/api/v1/platform/workspaces/{ws}",
        &admin,
        vector_platform.clone(),
        json!(["Vector renamed positional", false, cost_center])
    );
    body!(
        "PATCH /workspaces/{ws} (visibility only)",
        "PATCH",
        "/api/v1/workspaces/{ws}",
        &owner,
        vector_path.clone(),
        json!({"name":null,"object_visibility":"member"})
    );
    body!(
        "PATCH /workspaces/{ws} (positional, visibility only)",
        "PATCH",
        "/api/v1/workspaces/{ws}",
        &owner,
        vector_path.clone(),
        json!([null, "member"])
    );
    body!(
        "PATCH /workspaces/{ws} (positional)",
        "PATCH",
        "/api/v1/workspaces/{ws}",
        &owner,
        vector_path.clone(),
        json!(["Vector own positional", "workspace"])
    );
    body!(
        "PUT /platform/workspaces/{ws}/discoverable (positional)",
        "PUT",
        "/api/v1/platform/workspaces/{ws}/discoverable",
        &admin,
        format!("{vector_platform}/discoverable"),
        json!([true])
    );
    body!(
        "POST /platform/workspaces/{ws}/members (positional)",
        "POST",
        "/api/v1/platform/workspaces/{ws}/members",
        &admin,
        format!("{vector_platform}/members"),
        json!([vector_member.user_id, "member"])
    );
    body!(
        "POST /workspaces/{ws}/members (positional)",
        "POST",
        "/api/v1/workspaces/{ws}/members",
        &owner,
        format!("{vector_path}/members"),
        json!([vector_member2.user_id, "member"])
    );
    body!(
        "POST /workspaces/{ws}/invitations (positional)",
        "POST",
        "/api/v1/workspaces/{ws}/invitations",
        &owner,
        format!("{vector_path}/invitations"),
        json!(["vector-invite-positional@example.test", "member"])
    );
    body!(
        "POST /platform/workspaces/{ws}/models (positional)",
        "POST",
        "/api/v1/platform/workspaces/{ws}/models",
        &admin,
        format!("{vector_platform}/models"),
        json!([m])
    );
    body!(
        "PUT /platform/workspaces/{ws}/catalogs (positional)",
        "PUT",
        "/api/v1/platform/workspaces/{ws}/catalogs",
        &admin,
        format!("{vector_platform}/catalogs"),
        json!(["replace", [catalog]])
    );
    body!(
        "PUT /platform/workspace-types/{kind}/catalogs (positional)",
        "PUT",
        "/api/v1/platform/workspace-types/{kind}/catalogs",
        &admin,
        "/api/v1/platform/workspace-types/team/catalogs".to_owned(),
        json!([[catalog]])
    );
    reset_policy("/api/v1/platform/workspace-types/team/policy").await;
    reset_policy("/api/v1/platform/workspace-types/team/policy").await;
    body!(
        "PUT /platform/workspace-types/{kind}/policy (positional, nulls)",
        "PUT",
        "/api/v1/platform/workspace-types/{kind}/policy",
        &admin,
        "/api/v1/platform/workspace-types/team/policy".to_owned(),
        json!([null, null, null, null, null, null, null, null, "month"])
    );
    reset_policy(&format!("{vector_platform}/policy")).await;
    body!(
        "PUT /platform/workspaces/{ws}/policy (positional, nulls)",
        "PUT",
        "/api/v1/platform/workspaces/{ws}/policy",
        &admin,
        format!("{vector_platform}/policy"),
        json!([null, null, null, null, null, null, null, null, "month"])
    );
    body!(
        "PUT /platform/workspace-types/{kind}/policy (positional)",
        "PUT",
        "/api/v1/platform/workspace-types/{kind}/policy",
        &admin,
        "/api/v1/platform/workspace-types/team/policy".to_owned(),
        positional_policy.clone()
    );
    reset_policy(&format!("{vector_platform}/policy")).await;
    body!(
        "PUT /platform/workspaces/{ws}/policy (positional)",
        "PUT",
        "/api/v1/platform/workspaces/{ws}/policy",
        &admin,
        format!("{vector_platform}/policy"),
        positional_policy
    );
    for (kind, positional) in [
        (
            "platform",
            json!([
                ISSUER,
                "vector-positional",
                "platform",
                "user",
                null,
                null,
                true
            ]),
        ),
        (
            "workspace",
            json!([
                ISSUER,
                "vector-positional",
                "workspace",
                null,
                vector_ws,
                "member",
                true
            ]),
        ),
    ] {
        body!(
            &format!("POST /platform/oidc/group-mappings ({kind}, positional)"),
            "POST",
            "/api/v1/platform/oidc/group-mappings",
            &admin,
            "/api/v1/platform/oidc/group-mappings".to_owned(),
            positional.clone()
        );
        body!(
            &format!("POST /platform/oidc/group-mappings/preview ({kind}, positional)"),
            "POST",
            "/api/v1/platform/oidc/group-mappings/preview",
            &admin,
            "/api/v1/platform/oidc/group-mappings/preview".to_owned(),
            positional
        );
    }

    // Handler-derived probes the derived vectors cannot generate. Each pins the
    // schema's verdict AND the handler's verdict on the same input, so every
    // documented divergence (a byte limit against a character limit, trimming,
    // lower-casing, the stored row deciding a merge) stays visible.
    let probe_mapping = create_mapping_row(
        c,
        &admin,
        json!({"issuer":ISSUER,"group_value":"probe-group","target_kind":"platform","platform_role":"user","workspace_id":null,"workspace_role":null,"enabled":true}),
    )
    .await;
    let probe_path = format!("/api/v1/platform/oidc/group-mappings/{probe_mapping}");
    let probe_preview = format!("{probe_path}/preview");
    let mappings = "/api/v1/platform/oidc/group-mappings";
    let platform_with = |issuer: &str, group: &str| json!({"issuer":issuer,"group_value":group,"target_kind":"platform","platform_role":"user","workspace_id":null,"workspace_role":null,"enabled":true});
    let development = std::env::var("GATEWAY_ENV").as_deref() == Ok("development");
    c.guarded("issuer probes", async {
        let long_path = format!("https://issuer.example.test/{}", "\u{e9}".repeat(1011));
        let exact_path = format!("https://issuer.example.test/{}", "\u{e9}".repeat(1010));
        // (what, issuer, schema accepts, handler accepts)
        let probes: Vec<(&str, &str, bool, bool)> = vec![
            (
                "upper-case scheme",
                "HTTPS://issuer.example.test",
                true,
                true,
            ),
            ("port zero", "https://issuer.example.test:0", true, true),
            ("empty port", "https://issuer.example.test:", true, true),
            (
                "highest port",
                "https://issuer.example.test:65535",
                true,
                true,
            ),
            (
                "port above the highest",
                "https://issuer.example.test:65536",
                false,
                false,
            ),
            (
                "five-digit port above the highest",
                "https://issuer.example.test:99999",
                false,
                false,
            ),
            (
                "port with leading zeros",
                "https://issuer.example.test:000080",
                false,
                true,
            ),
            (
                "percent sign in the host",
                "https://iss%er.example.test",
                false,
                false,
            ),
            (
                "percent-encoded host character",
                "https://iss%75er.example.test",
                false,
                true,
            ),
            (
                "angle bracket in the host",
                "https://iss<er.example.test",
                false,
                false,
            ),
            (
                "caret in the host",
                "https://iss^er.example.test",
                false,
                false,
            ),
            (
                "pipe in the host",
                "https://iss|er.example.test",
                false,
                false,
            ),
            (
                "backslash ends the host",
                "https://iss\\er.example.test",
                false,
                true,
            ),
            (
                "backslash path separator",
                "https://issuer.example.test\\path",
                false,
                true,
            ),
            (
                "scheme without slashes",
                "https:issuer.example.test",
                false,
                true,
            ),
            (
                "scheme with one slash",
                "https:/issuer.example.test",
                false,
                true,
            ),
            (
                "scheme with three slashes",
                "https:///issuer.example.test",
                false,
                true,
            ),
            (
                "leading whitespace",
                " https://issuer.example.test",
                false,
                true,
            ),
            (
                "trailing whitespace",
                "https://issuer.example.test ",
                false,
                true,
            ),
            (
                "tab inside the host",
                "https://iss\tuer.example.test",
                false,
                true,
            ),
            (
                "space in the path",
                "https://issuer.example.test/a b",
                false,
                true,
            ),
            (
                "C0 control character in the path",
                "https://issuer.example.test/a\u{1}b",
                false,
                true,
            ),
            (
                "C1 control character in the path",
                "https://issuer.example.test/a\u{85}b",
                false,
                true,
            ),
            (
                "no-break space in the path",
                "https://issuer.example.test/a\u{a0}b",
                false,
                true,
            ),
            (
                "empty user info",
                "https://@issuer.example.test",
                false,
                true,
            ),
            (
                "numeric host outside the IPv4 range",
                "https://256.1.1.1",
                true,
                false,
            ),
            (
                "bracketed host that is not IPv6",
                "https://[:::]",
                true,
                false,
            ),
            ("empty query", "https://issuer.example.test?", false, false),
            (
                "empty fragment",
                "https://issuer.example.test#",
                false,
                false,
            ),
            (
                "user info",
                "https://user@issuer.example.test",
                false,
                false,
            ),
            (
                "http outside development",
                "http://localhost",
                false,
                development,
            ),
            (
                "multibyte path above 2048 bytes",
                long_path.as_str(),
                true,
                false,
            ),
            (
                "multibyte path of exactly 2048 bytes",
                exact_path.as_str(),
                true,
                true,
            ),
        ];
        for (what, issuer, schema, handler) in probes {
            c.pin_body(
                &admin,
                "POST",
                &format!("{mappings}/preview"),
                what,
                platform_with(issuer, "probe-group"),
                schema,
                handler,
            )
            .await;
        }
    })
    .await;
    c.guarded("group value probes", async {
        let wide = "\u{20ac}".repeat(171);
        let exact = "\u{20ac}".repeat(170);
        for (method, path) in [
            ("POST", mappings.to_owned()),
            ("POST", format!("{mappings}/preview")),
        ] {
            c.pin_body(
                &admin,
                method,
                &path,
                "group value above 512 bytes",
                platform_with(ISSUER, &wide),
                true,
                false,
            )
            .await;
        }
        c.pin_body(
            &admin,
            "POST",
            &format!("{mappings}/preview"),
            "group value of exactly 510 bytes",
            platform_with(ISSUER, &exact),
            true,
            true,
        )
        .await;
        c.pin_body(
            &admin,
            "PATCH",
            &probe_path,
            "patched group value above 512 bytes",
            json!({"group_value":wide}),
            true,
            false,
        )
        .await;
        c.pin_body(
            &admin,
            "POST",
            &probe_preview,
            "previewed group value above 512 bytes",
            json!({"group_value":wide}),
            true,
            false,
        )
        .await;
    })
    .await;
    c.guarded("mapping variant probes", async {
        let workspace = |kind: &str, role: Value, id: Value, ws_role: Value| {
            json!({"issuer":ISSUER,"group_value":"probe-group","target_kind":kind,"platform_role":role,"workspace_id":id,"workspace_role":ws_role,"enabled":true})
        };
        let new_preview = format!("{mappings}/preview");
        let wid = json!(vector_ws);
        for (what, body, schema, handler) in [
            ("workspace kind with a platform role", workspace("workspace", json!("user"), wid.clone(), json!("member")), false, false),
            ("platform kind with workspace fields", workspace("platform", json!("user"), wid.clone(), json!("member")), false, false),
            ("workspace kind without a workspace role", workspace("workspace", Value::Null, wid.clone(), Value::Null), false, false),
            ("platform kind without a platform role", workspace("platform", Value::Null, Value::Null, Value::Null), false, false),
            ("workspace kind with its fields", workspace("workspace", Value::Null, wid.clone(), json!("member")), true, true),
        ] {
            c.pin_body(&admin, "POST", &new_preview, what, body, schema, handler).await;
        }
        // The merge onto the stored platform mapping: first as a stateless preview,
        // then as the real PATCH (the last accepted body switches the row).
        let patches: Vec<(&str, Value, bool, bool)> = vec![
            ("workspace kind keeping the platform role", json!({"target_kind":"workspace","platform_role":"user"}), false, false),
            ("workspace kind with every field and a platform role", json!({"target_kind":"workspace","platform_role":"user","workspace_id":vector_ws,"workspace_role":"member"}), false, false),
            ("platform kind with a workspace id", json!({"target_kind":"platform","workspace_id":vector_ws}), false, false),
            ("platform kind with a workspace role", json!({"target_kind":"platform","workspace_role":"member"}), false, false),
            ("platform kind with a null platform role", json!({"target_kind":"platform","platform_role":null}), false, false),
            ("workspace kind with a null workspace id", json!({"target_kind":"workspace","platform_role":null,"workspace_id":null,"workspace_role":"member"}), false, false),
            ("workspace kind with a null workspace role", json!({"target_kind":"workspace","platform_role":null,"workspace_id":vector_ws,"workspace_role":null}), false, false),
            // Without target_kind the stored row decides, which the schema cannot see.
            ("workspace id without a kind on a platform mapping", json!({"workspace_id":vector_ws}), true, false),
            ("null platform role without a kind on a platform mapping", json!({"platform_role":null}), true, false),
            // Fields the body omits are filled from the stored row even with target_kind.
            ("workspace kind leaving the stored platform role", json!({"target_kind":"workspace","workspace_id":vector_ws,"workspace_role":"member"}), true, false),
            ("platform kind restated", json!({"target_kind":"platform","platform_role":"auditor","workspace_id":null,"workspace_role":null}), true, true),
            ("switch to a workspace mapping", json!({"target_kind":"workspace","platform_role":null,"workspace_id":vector_ws,"workspace_role":"admin"}), true, true),
        ];
        for (what, body, schema, handler) in &patches {
            c.pin_body(&admin, "POST", &probe_preview, what, body.clone(), *schema, *handler).await;
        }
        for (what, body, schema, handler) in &patches {
            c.pin_body(&admin, "PATCH", &probe_path, what, body.clone(), *schema, *handler).await;
        }
    })
    .await;
    c.guarded("delete preview probes", async {
        let full = json!({"issuer":ISSUER,"group_value":"probe-group","target_kind":"workspace","platform_role":null,"workspace_id":vector_ws,"workspace_role":"member","enabled":true});
        let mut with_delete = full.clone();
        with_delete["delete"] = json!(true);
        // `is_delete = delete || no mapping field`; with delete:true the other
        // fields are ignored, which the schema (typing them) cannot express.
        for (what, body, schema, handler) in [
            ("empty body", json!({}), true, true),
            ("delete flag false alone", json!({"delete":false}), true, true),
            ("delete flag true alone", json!({"delete":true}), true, true),
            ("delete flag true with every field", with_delete, true, true),
            ("delete flag true with ignored invalid fields", json!({"delete":true,"enabled":"x","issuer":1}), false, true),
            ("delete flag true with an ignored cross-field conflict", json!({"delete":true,"target_kind":"platform","workspace_id":vector_ws}), false, true),
            ("delete flag true with an unknown key", json!({"delete":true,"unexpected_configuration":true}), false, false),
            ("delete flag false with an invalid field", json!({"delete":false,"issuer":1}), false, false),
        ] {
            c.pin_body(&admin, "POST", &probe_preview, what, body, schema, handler).await;
        }
    })
    .await;
    // Email: the handler counts UTF-8 bytes of the trimmed, lower-cased text.
    c.guarded("email probes", async {
        let wide = format!("{}@t.test", "\u{e9}".repeat(157));
        let expanding = format!("{}@{}", "\u{130}".repeat(100), "a".repeat(20));
        type Body = Box<dyn Fn(&str) -> Value>;
        let targets: Vec<(&str, &str, String, Body)> = vec![
            (
                "user create",
                "POST",
                "/api/v1/platform/users".to_owned(),
                Box::new(|e: &str| json!({"email":e,"platform_role":"user"})),
            ),
            (
                "user update",
                "PATCH",
                format!("/api/v1/platform/users/{}", vector_user.user_id),
                Box::new(|e: &str| json!({"email":e})),
            ),
            (
                "invitation",
                "POST",
                format!("{vector_path}/invitations"),
                Box::new(|e: &str| json!({"email":e,"role":"member"})),
            ),
        ];
        for (i, (name, method, path, body)) in targets.iter().enumerate() {
            let fill = ["p", "q", "r"][i];
            let padded = format!("  {}@t.test  ", fill.repeat(313));
            let who = if *name == "invitation" {
                &owner
            } else {
                &admin
            };
            c.pin_body(
                who,
                method,
                path,
                &format!("{name} email padded beyond 320 characters"),
                body(&padded),
                false,
                true,
            )
            .await;
            c.pin_body(
                who,
                method,
                path,
                &format!("{name} email above 320 bytes in fewer characters"),
                body(&wide),
                true,
                false,
            )
            .await;
            c.pin_body(
                who,
                method,
                path,
                &format!("{name} email lengthened by lower-casing"),
                body(&expanding),
                true,
                false,
            )
            .await;
        }
    })
    .await;
    // Workspace names: UTF-8 bytes of the value as sent.
    c.guarded("workspace name probes", async {
        let owner_id = f.owner.user_id;
        let wide = "\u{20ac}".repeat(41);
        let targets: Vec<(&str, &str, String, &Browser, Value)> = vec![
            (
                "create",
                "POST",
                "/api/v1/platform/workspaces".to_owned(),
                &admin,
                json!({"name":wide,"kind":"project","owner_user_id":owner_id}),
            ),
            (
                "platform rename",
                "PATCH",
                vector_platform.clone(),
                &admin,
                json!({"name":wide}),
            ),
            (
                "own rename",
                "PATCH",
                vector_path.clone(),
                &owner,
                json!({"name":wide}),
            ),
        ];
        for (name, method, path, who, body) in targets {
            c.pin_body(
                who,
                method,
                &path,
                &format!("workspace {name} above 120 bytes in fewer characters"),
                body,
                true,
                false,
            )
            .await;
        }
    })
    .await;
    // Query values: bytes against characters, trimming and lower-casing.
    c.guarded("query probes", async {
        let padded = format!("{}  ", "a".repeat(200));
        let expanding = "\u{130}".repeat(150);
        for (who, template, path) in [
            (
                &auditor,
                "/api/v1/platform/workspaces/{ws}/members",
                format!("{platform}/members"),
            ),
            (
                &owner,
                "/api/v1/workspaces/{ws}/members",
                format!("{own}/members"),
            ),
        ] {
            c.pin_query(
                who,
                template,
                &path,
                "members q padded beyond 200 characters",
                &[("q", padded.as_str())],
                false,
                true,
            )
            .await;
        }
        for (who, template, path) in [
            (
                &auditor,
                "/api/v1/platform/workspaces/{ws}/member-candidates",
                format!("{platform}/member-candidates"),
            ),
            (
                &owner,
                "/api/v1/workspaces/{ws}/member-candidates",
                format!("{own}/member-candidates"),
            ),
        ] {
            c.pin_query(
                who,
                template,
                &path,
                "candidates q padded beyond 200 characters",
                &[("q", padded.as_str())],
                false,
                true,
            )
            .await;
            c.pin_query(
                who,
                template,
                &path,
                "candidates q lengthened by lower-casing",
                &[("q", expanding.as_str())],
                true,
                false,
            )
            .await;
        }
        let wide = "\u{e9}".repeat(101);
        let exact = "\u{e9}".repeat(100);
        for (template, path) in [
            ("/api/v1/workspaces/{ws}/sessions", session_path.clone()),
            (
                "/api/v1/workspaces/{ws}/sessions/{session}",
                detail_path.clone(),
            ),
        ] {
            c.pin_query(
                &owner,
                template,
                &path,
                "model above 200 bytes in fewer characters",
                &[("model", wide.as_str())],
                true,
                false,
            )
            .await;
            c.pin_query(
                &owner,
                template,
                &path,
                "model of exactly 200 bytes",
                &[("model", exact.as_str())],
                true,
                true,
            )
            .await;
        }
    })
    .await;

    // Exact: a whole probe group dropping out, or accepts silently turning into
    // domain refusals, must fail. Only counts are reported, never values.
    let (pins, domain) = (c.pins.get(), c.domain_pins.get());
    c.expect(
        pins == EXPECTED_PINS && domain == EXPECTED_DOMAIN_PINS,
        &format!(
            "handler-derived probes: {pins} ran ({domain} accepted via 404/409); expected {EXPECTED_PINS} ({EXPECTED_DOMAIN_PINS})"
        ),
    );
    let mut failures = c.failures.borrow().clone();
    // The coverage gates last: they judge every positive and vector set above.
    let observed = c.observed.lock().await;
    let gates = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        observed.finish(SLICE2_AREAS);
    }));
    if let Err(panic) = gates {
        failures.push(
            panic
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| panic.downcast_ref::<&str>().map(|s| (*s).to_owned()))
                .unwrap_or_else(|| "gates panicked".to_owned()),
        );
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
