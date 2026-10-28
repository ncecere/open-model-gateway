//! Every Slice 2 operation is executed over the real loopback serving stack.
//! Fixture credentials stay in memory; neither tokens nor response bodies are logged.
use super::*;
use openapi::responses::{Captured, PositiveFixtures, SLICE2_AREAS, capture_http};
use sha2::{Digest, Sha256};

const ORIGIN: &str = "http://127.0.0.1:3000";
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
struct Client {
    http: reqwest::Client,
    base: String,
    observed: PositiveFixtures,
}
impl Client {
    async fn send(
        &mut self,
        who: &Browser,
        method: &str,
        path: &str,
        input: Option<Value>,
        expected: StatusCode,
    ) -> Value {
        let mut request = self
            .http
            .request(method.parse().unwrap(), format!("{}{path}", self.base))
            .header("origin", ORIGIN)
            .header("cookie", format!("omg_session={}", who.token))
            .header("x-csrf-token", &who.csrf);
        if let Some(input) = input {
            if expected == StatusCode::OK {
                openapi::responses::check_request(method, path, &input).unwrap();
            }
            request = request.json(&input);
        }
        let r: Captured = capture_http(method, path, request.send().await.unwrap()).await;
        assert_eq!(r.status, expected, "{method} {path}");
        self.observed.observe(&r);
        serde_json::from_slice(&r.bytes).unwrap_or(Value::Null)
    }
    async fn ok(&mut self, who: &Browser, method: &str, path: &str, input: Option<Value>) -> Value {
        self.send(who, method, path, input, StatusCode::OK).await
    }
    /// A refused body, pinned against the documented request schema: a
    /// STRUCTURAL refusal (wrong type, null, unknown field, wrong root) must
    /// also be rejected by the schema (`schema_accepts == false`); a SEMANTIC
    /// refusal (value rules the handler applies after parsing) is accepted by
    /// the schema and listed with `schema_accepts == true`. Only the verdict
    /// is compared; the body is never printed.
    async fn refused(
        &mut self,
        who: &Browser,
        method: &str,
        path: &str,
        body: Value,
        status: StatusCode,
        schema_accepts: bool,
    ) {
        assert_eq!(
            openapi::responses::check_request(method, path, &body).is_ok(),
            schema_accepts,
            "{method} {path} {status}: request schema verdict disagrees with the handler class"
        );
        self.send(who, method, path, Some(body), status).await;
    }
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
    let mut c = Client {
        http: reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap(),
        base,
        observed: PositiveFixtures::default(),
    };

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
    let users = c.ok(&auditor, "GET", "/api/v1/platform/users", None).await;
    assert!(!users["data"].as_array().unwrap().is_empty());
    c.ok(&auditor, "GET", &up, None).await;
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
    let created = c
        .ok(
            &admin,
            "POST",
            "/api/v1/platform/workspaces",
            Some(
                json!({"name":"Contract shared","kind":"project","owner_user_id":f.owner.user_id}),
            ),
        )
        .await;
    let ws = id(&created);
    let platform = format!("/api/v1/platform/workspaces/{ws}");
    let own = format!("/api/v1/workspaces/{ws}");
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
        Some(json!({"name":"Contract renamed","disabled":false,"cost_center_id":null})),
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
    assert!(c.ok(&auditor, "GET", &own, None).await["role"].is_null());
    let personal = c
        .ok(
            &owner,
            "GET",
            &format!("/api/v1/workspaces/{}", f.personal),
            None,
        )
        .await;
    assert!(personal.get("object_visibility").is_none());
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
        &format!("/api/v1/workspaces/{}", f.personal),
        Some(json!({"name":"not allowed"})),
        StatusCode::CONFLICT,
    )
    .await;
    c.send(&outsider, "GET", &own, None, StatusCode::FORBIDDEN)
        .await;

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
    let mapping_body = json!({"issuer":"https://issuer.example.test","group_value":"contract-group","target_kind":"workspace","platform_role":null,"workspace_id":ws,"workspace_role":"member","enabled":true});
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
    // Structural refusals of the manual PATCH parser: the schema must reject
    // each of them too. `{"enabled":null}`/`{"issuer":null}` are rejected because
    // PATCH merges and re-deserializes MappingInput (only the three role/workspace
    // fields are nullable); the semantic rows are accepted by the schema.
    for (probe, status, schema_accepts) in [
        (
            json!({"unexpected_configuration":true}),
            StatusCode::UNPROCESSABLE_ENTITY,
            false,
        ),
        (
            json!({"enabled":{"unexpected_configuration":true}}),
            StatusCode::BAD_REQUEST,
            false,
        ),
        (json!({"enabled":null}), StatusCode::BAD_REQUEST, false),
        (json!({"enabled":"true"}), StatusCode::BAD_REQUEST, false),
        (json!({"issuer":null}), StatusCode::BAD_REQUEST, false),
        (json!({"group_value":null}), StatusCode::BAD_REQUEST, false),
        (json!({"target_kind":null}), StatusCode::BAD_REQUEST, false),
        (
            json!({"workspace_id":"not-a-uuid"}),
            StatusCode::BAD_REQUEST,
            false,
        ),
        (json!([]), StatusCode::BAD_REQUEST, false),
        (Value::Null, StatusCode::BAD_REQUEST, false),
        // Semantic: a well-typed target_kind the handler refuses.
        (
            json!({"target_kind":"bogus"}),
            StatusCode::BAD_REQUEST,
            true,
        ),
    ] {
        c.refused(
            &admin,
            "PATCH",
            &mapping_path,
            probe,
            status,
            schema_accepts,
        )
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
    for (probe, status, schema_accepts) in [
        (
            json!({"unexpected_configuration":true}),
            StatusCode::UNPROCESSABLE_ENTITY,
            false,
        ),
        (json!({"delete":null}), StatusCode::BAD_REQUEST, false),
        (json!({"delete":[]}), StatusCode::BAD_REQUEST, false),
        (json!({"delete":"true"}), StatusCode::BAD_REQUEST, false),
        (json!({"enabled":null}), StatusCode::BAD_REQUEST, false),
        (json!({"issuer":null}), StatusCode::BAD_REQUEST, false),
        (
            json!({"enabled":{"unexpected_configuration":true}}),
            StatusCode::BAD_REQUEST,
            false,
        ),
        (json!([]), StatusCode::BAD_REQUEST, false),
        (Value::Null, StatusCode::BAD_REQUEST, false),
        // Semantic: a well-typed target_kind the shared merge validator refuses.
        (
            json!({"target_kind":"bogus"}),
            StatusCode::BAD_REQUEST,
            true,
        ),
    ] {
        c.refused(
            &admin,
            "POST",
            &preview_path,
            probe,
            status,
            schema_accepts,
        )
        .await;
    }
    // The NEW-mapping preview is a typed DTO whose fields are all Option: null is
    // structurally accepted (schema accepts) and the handler then refuses the
    // missing required value (semantic 400), unlike the merge-based bodies above.
    for field in ["issuer", "group_value", "target_kind", "enabled"] {
        let mut body = json!({"issuer":"https://issuer.example.test","group_value":"contract-group","target_kind":"platform","platform_role":"user","workspace_id":null,"workspace_role":null,"enabled":true});
        body[field] = Value::Null;
        c.refused(
            &admin,
            "POST",
            "/api/v1/platform/oidc/group-mappings/preview",
            body,
            StatusCode::BAD_REQUEST,
            true,
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
    // Semantic: well-typed bodies that name nothing to change (null is the
    // same as omission) are accepted by the schema and refused by the handler.
    for body in [json!({}), json!({"object_visibility":null})] {
        c.refused(&owner, "PATCH", &own, body, StatusCode::BAD_REQUEST, true)
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
    }
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
    let preview=c.ok(&admin,"POST","/api/v1/platform/oidc/group-mappings/preview",Some(json!({"issuer":"https://issuer.example.test","group_value":"contract-group","target_kind":"platform","platform_role":"admin","workspace_id":null,"workspace_role":null,"enabled":true}))).await;
    assert!(preview["digest"].is_null());
    assert!(
        !preview["diff"]["platform_role_gained"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let preview = c
        .ok(
            &admin,
            "POST",
            &format!("/api/v1/platform/oidc/group-mappings/{mapping}/preview"),
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
    c.send(
        &auditor,
        "POST",
        "/api/v1/platform/oidc/group-mappings/preview",
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
    c.ok(
        &admin,
        "PATCH",
        &format!("/api/v1/platform/oidc/group-mappings/{mapping}"),
        Some(json!({"enabled":false})),
    )
    .await;
    c.ok(
        &admin,
        "DELETE",
        &format!("/api/v1/platform/oidc/group-mappings/{mapping}"),
        None,
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

    // Access requests: both queues and all four decision operations, including
    // current-eligibility redaction and the requester-private decision projection.
    c.ok(
        &admin,
        "PUT",
        &format!("{platform}/discoverable"),
        Some(json!({"discoverable":true})),
    )
    .await;
    for (i, scope, outcome) in [
        (0, "platform", "approve"),
        (1, "platform", "deny"),
        (2, "workspace", "approve"),
        (3, "workspace", "deny"),
    ] {
        let requester = user(&pool, &format!("requester-{i}"), "user").await;
        let request_browser = browser(&pool, &requester).await;
        let request=id(&c.ok(&request_browser,"POST","/api/v1/me/access-requests",Some(json!({"workspace_id":ws,"reason":"Contract request","idempotency_key":format!("slice2-{i}")}))).await);
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
        if i == 3 {
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

    // Catalog and policy layers: populated replacement/default lists, never a union.
    let catalog = Uuid::new_v4();
    sqlx::query("INSERT INTO catalogs(id,name) VALUES($1,'Contract catalog')")
        .bind(catalog)
        .execute(&pool)
        .await
        .unwrap();
    let m = model(&pool, "contract-access-model").await;
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
    c.ok(&admin, "DELETE", &format!("{platform}/catalogs"), None)
        .await;
    c.ok(
        &admin,
        "POST",
        &format!("{platform}/models"),
        Some(json!({"model_id":m})),
    )
    .await;
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
    c.ok(&admin, "PUT", &format!("{platform}/policy"), Some(limits))
        .await;
    assert!(
        !c.ok(&auditor, "GET", &format!("{platform}/policy"), None)
            .await["budgets"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(
        !c.ok(&owner, "GET", &format!("{own}/access"), None).await["models"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    c.ok(&admin, "DELETE", &format!("{platform}/policy"), None)
        .await;
    c.ok(&admin, "DELETE", &format!("{platform}/models/{m}"), None)
        .await;

    // Nonempty inference session reads with unknown usage, including own-vs-wide refusal.
    let connection = Uuid::new_v4();
    let deployment = Uuid::new_v4();
    sqlx::query("INSERT INTO provider_connections(id,name,provider,credential_ref) VALUES($1,'Contract mock','openai','env:UNUSED')").bind(connection).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO deployments(id,model_id,provider_connection_id,upstream_model) VALUES($1,$2,$3,'mock')").bind(deployment).bind(m).bind(connection).execute(&pool).await.unwrap();
    let (status, key_value) = key(&f, &f.owner, f.team, Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    let execution = Uuid::new_v4();
    sqlx::query("INSERT INTO inference_executions(id,workspace_id,api_key_id,deployment_id,public_model,provider,streamed,state,root_request_id,client_session_id) VALUES($1,$2,$3,$4,'contract-access-model','openai',false,'succeeded',$1,'contract-session')").bind(execution).bind(f.team).bind(id(&key_value)).bind(deployment).execute(&pool).await.unwrap();
    let session_path = format!("/api/v1/workspaces/{}/sessions", f.team);
    assert!(
        !c.ok(&owner, "GET", &session_path, None).await["data"]
            .as_array()
            .unwrap()
            .is_empty()
    );
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

    // Platform subject export is a distinct, closed privacy projection.
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
    c.observed.finish(SLICE2_AREAS);
}
