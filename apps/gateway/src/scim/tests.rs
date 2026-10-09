use super::*;
use axum::{body::Body, http::Method};
use tower::ServiceExt;

const TOKEN: &str = "scim-test-token-0123456789abcdefghijklmnop";
const ISSUER: &str = "https://issuer.test";
const BASE: &str = "https://gateway.test/scim/v2";
const PATCH_OP: &str = "urn:ietf:params:scim:api:messages:2.0:PatchOp";

fn runtime() -> Arc<ScimRuntime> {
    Arc::new(ScimRuntime {
        token_hash: Sha256::digest(TOKEN.as_bytes()).into(),
        issuer: ISSUER.into(),
        base_url: BASE.into(),
    })
}

fn lazy_pool() -> sqlx::PgPool {
    sqlx::postgres::PgPoolOptions::new()
        .connect_lazy("postgres://unused:unused@127.0.0.1:1/unused")
        .unwrap()
}

fn app(pool: sqlx::PgPool, enabled: bool) -> Router {
    let store = Store::new(pool);
    router(ScimState::new(store.clone(), enabled.then(runtime))).with_state(store)
}

async fn send(
    app: &Router,
    method: Method,
    uri: &str,
    auth: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, HeaderMap, Value) {
    let mut request = axum::http::Request::builder().method(method).uri(uri);
    if let Some(auth) = auth {
        request = request.header(header::AUTHORIZATION, auth);
    }
    let body = match body {
        Some(body) => {
            request = request.header(header::CONTENT_TYPE, "application/scim+json");
            Body::from(body.to_string())
        }
        None => Body::empty(),
    };
    let response = app
        .clone()
        .oneshot(request.body(body).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 22)
        .await
        .unwrap();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    (status, headers, value)
}

fn bearer() -> String {
    format!("Bearer {TOKEN}")
}

#[test]
fn token_configuration_is_a_reference_and_strict() {
    let env = |pairs: &'static [(&'static str, &'static str)]| {
        move |name: &str| {
            pairs
                .iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| (*v).to_owned())
        }
    };
    assert!(ScimConfig::from_lookup(env(&[])).unwrap().is_none());
    let config = ScimConfig::from_lookup(env(&[
        ("GATEWAY_SCIM_TOKEN_ENV", "OKTA_SCIM_TOKEN"),
        ("OKTA_SCIM_TOKEN", TOKEN),
    ]))
    .unwrap()
    .unwrap();
    assert_eq!(
        config.token_hash(),
        <[u8; 32]>::from(Sha256::digest(TOKEN.as_bytes()))
    );
    for bad in [
        &[("GATEWAY_SCIM_TOKEN_ENV", "")][..],
        &[("GATEWAY_SCIM_TOKEN_ENV", "lower_case")],
        &[("GATEWAY_SCIM_TOKEN_ENV", "1ABC")],
        &[("GATEWAY_SCIM_TOKEN_ENV", "GATEWAY_SCIM_TOKEN_ENV")],
        &[("GATEWAY_SCIM_TOKEN_ENV", "MISSING_TOKEN")],
        &[
            ("GATEWAY_SCIM_TOKEN_ENV", "SHORT_TOKEN"),
            ("SHORT_TOKEN", "too-short"),
        ],
        &[
            ("GATEWAY_SCIM_TOKEN_ENV", "SPACED_TOKEN"),
            (
                "SPACED_TOKEN",
                "has a space in it but is long enough 0123456789",
            ),
        ],
    ] {
        let error = match ScimConfig::from_lookup(env(bad)) {
            Ok(_) => panic!("{bad:?} accepted"),
            Err(error) => error.to_string(),
        };
        // Errors name the variable, never echo a token value.
        assert!(!error.contains("too-short") && !error.contains("has a space"));
    }
}

#[tokio::test]
async fn disabled_scim_is_not_found_everywhere() {
    let app = app(lazy_pool(), false);
    for (method, path) in [
        (Method::GET, "/scim"),
        (Method::GET, "/scim/v2"),
        (Method::GET, "/scim/v2/ServiceProviderConfig"),
        (Method::PUT, "/scim/v2/ServiceProviderConfig"),
        (Method::GET, "/scim/v2/Users?filter=userName%20eq%20%22a%22"),
        (Method::POST, "/scim/v2/Users"),
        (
            Method::PATCH,
            "/scim/v2/Users/00000000-0000-0000-0000-000000000000",
        ),
        (Method::DELETE, "/scim/v2/Groups/x"),
        (Method::GET, "/scim/anything/else"),
    ] {
        let (status, headers, _) = send(&app, method.clone(), path, Some(&bearer()), None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{method} {path}");
        assert_eq!(headers[header::CONTENT_TYPE], "application/json");
        assert!(headers.get(header::WWW_AUTHENTICATE).is_none());
    }
}

#[tokio::test]
async fn bearer_token_is_required_and_compared_by_hash() {
    let app = app(lazy_pool(), true);
    let wrong = format!("Bearer {TOKEN}x");
    let basic = format!("Basic {TOKEN}");
    let no_space = format!("Bearer{TOKEN}");
    for auth in [
        None,
        Some(wrong.as_str()),
        Some(basic.as_str()),
        Some(no_space.as_str()),
        Some("Bearer "),
        Some("Bearer"),
    ] {
        for path in [
            "/scim/v2/ServiceProviderConfig",
            "/scim/v2/Users",
            "/scim/nope",
        ] {
            let (status, headers, body) = send(&app, Method::GET, path, auth, None).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "{auth:?} {path}");
            assert_eq!(headers[header::WWW_AUTHENTICATE], "Bearer realm=\"scim\"");
            assert_eq!(headers[header::CONTENT_TYPE], "application/scim+json");
            assert_eq!(body["schemas"][0], ERROR_SCHEMA);
            assert_eq!(body["status"], "401");
            assert!(!body.to_string().contains(TOKEN));
        }
    }
    // Duplicate Authorization headers are ambiguous: rejected.
    let request = axum::http::Request::builder()
        .uri("/scim/v2/ServiceProviderConfig")
        .header(header::AUTHORIZATION, bearer())
        .header(header::AUTHORIZATION, bearer())
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        app.clone().oneshot(request).await.unwrap().status(),
        StatusCode::UNAUTHORIZED
    );
    // Scheme is case-insensitive; discovery needs no database.
    let lower = format!("bearer {TOKEN}");
    let (status, headers, spc) = send(
        &app,
        Method::GET,
        "/scim/v2/ServiceProviderConfig",
        Some(&lower),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CACHE_CONTROL], "no-store");
    assert_eq!(spc["patch"]["supported"], true);
    assert_eq!(spc["bulk"]["supported"], false);
    assert_eq!(spc["filter"]["maxResults"], 200);
    assert_eq!(spc["authenticationSchemes"][0]["type"], "oauthbearertoken");
    let (_, _, types) = send(
        &app,
        Method::GET,
        "/scim/v2/ResourceTypes",
        Some(&bearer()),
        None,
    )
    .await;
    assert_eq!(types["totalResults"], 2);
    assert_eq!(types["Resources"][0]["endpoint"], "/Users");
    let (_, _, group_type) = send(
        &app,
        Method::GET,
        "/scim/v2/ResourceTypes/Group",
        Some(&bearer()),
        None,
    )
    .await;
    assert_eq!(group_type["schema"], GROUP_SCHEMA);
    let (_, _, schemas) = send(&app, Method::GET, "/scim/v2/Schemas", Some(&bearer()), None).await;
    assert_eq!(schemas["Resources"][0]["id"], USER_SCHEMA);
    let (status, _, user_schema) = send(
        &app,
        Method::GET,
        &format!("/scim/v2/Schemas/{USER_SCHEMA}"),
        Some(&bearer()),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(user_schema["attributes"][0]["name"], "userName");
    let (status, _, _) = send(
        &app,
        Method::GET,
        "/scim/v2/Schemas/unknown",
        Some(&bearer()),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _, missing) =
        send(&app, Method::GET, "/scim/v2/Bulk", Some(&bearer()), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(missing["schemas"][0], ERROR_SCHEMA);
}

#[test]
fn filters_are_a_strict_eq_subset() {
    assert_eq!(
        parse_filter(r#"userName eq "Alice@Example.com""#).unwrap(),
        ("username".into(), "Alice@Example.com".into())
    );
    assert_eq!(
        parse_filter(&format!(r#"{USER_SCHEMA}:externalId EQ "a\"b""#)).unwrap(),
        ("externalid".into(), "a\"b".into())
    );
    for bad in [
        "userName co \"a\"",
        "userName eq a",
        "userName eq \"a\" and active eq true",
        "userName",
        "active eq true",
    ] {
        assert_eq!(
            parse_filter(bad).unwrap_err().scim_type,
            Some("invalidFilter"),
            "{bad}"
        );
    }
}

fn draft() -> UserDraft {
    UserDraft {
        user_name: "ada@example.com".into(),
        external_id: Some("00u1".into()),
        given_name: Some("Ada".into()),
        family_name: Some("Lovelace".into()),
        display_name: Some("Ada Lovelace".into()),
        email: "ada@example.com".into(),
        active: true,
    }
}

#[test]
fn user_patch_semantics_cover_okta_and_entra_shapes() {
    let mut user = draft();
    // Okta: path-less replace.
    apply_user_patch(
        &mut user,
        &json!({"schemas":[PATCH_OP],"Operations":[{"op":"replace","value":{"active":false}}]}),
    )
    .unwrap();
    assert!(!user.active);
    // Entra: capitalized op, string booleans, paths, filtered email path, extension ignored.
    apply_user_patch(&mut user, &json!({"Operations":[
        {"op":"Replace","path":"active","value":"True"},
        {"op":"Replace","path":"displayName","value":"  Ada L.  "},
        {"op":"Replace","path":"name.givenName","value":"Augusta"},
        {"op":"Replace","path":"emails[type eq \"work\"].value","value":"ADA.L@Example.com"},
        {"op":"Add","path":"urn:ietf:params:scim:schemas:extension:enterprise:2.0:User:department","value":"R&D"},
        {"op":"Add","path":"title","value":"Countess"},
        {"op":"Remove","path":"externalId"},
        {"op":"Replace","value":{"name.familyName":"King","urn:ietf:params:scim:schemas:core:2.0:User:userName":"ada.l@example.com"}}
    ]}))
    .unwrap();
    assert_eq!(
        user,
        UserDraft {
            user_name: "ada.l@example.com".into(),
            external_id: None,
            given_name: Some("Augusta".into()),
            family_name: Some("King".into()),
            display_name: Some("Ada L.".into()),
            email: "ada.l@example.com".into(),
            active: true,
        }
    );
    let before = user.clone();
    for (body, kind) in [
        (json!({"Operations":[]}), "invalidSyntax"),
        (json!({}), "invalidSyntax"),
        (
            json!({"Operations":[{"op":"move","path":"active"}]}),
            "invalidSyntax",
        ),
        (
            json!({"Operations":[{"op":"replace","path":"active","value":"maybe"}]}),
            "invalidValue",
        ),
        (
            json!({"Operations":[{"op":"replace","path":"emails","value":[{"value":"not-an-email"}]}]}),
            "invalidValue",
        ),
        (
            json!({"Operations":[{"op":"remove","path":"userName"}]}),
            "mutability",
        ),
        (
            json!({"Operations":[{"op":"remove","path":"emails"}]}),
            "mutability",
        ),
        (json!({"Operations":[{"op":"remove"}]}), "noTarget"),
        (
            json!({"Operations":[{"op":"replace","value":"x"}]}),
            "invalidSyntax",
        ),
        (
            json!({"Operations":[{"op":"replace","path":"displayName","value":"x".repeat(201)}]}),
            "invalidValue",
        ),
        (
            json!({"Operations":[{"op":"replace","path":"displayName","value":"bad\u{7}"}]}),
            "invalidValue",
        ),
    ] {
        let mut copy = before.clone();
        assert_eq!(
            apply_user_patch(&mut copy, &body).unwrap_err().scim_type,
            Some(kind),
            "{body}"
        );
    }
}

#[test]
fn group_patch_semantics_cover_member_paths() {
    let (a, b, c) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    let mut group = GroupDraft {
        display_name: "Engineering".into(),
        external_id: None,
        members: BTreeSet::from([a]),
    };
    apply_group_patch(
        &mut group,
        &json!({"Operations":[
            {"op":"add","path":"members","value":[{"value":b.to_string()},{"value":c.to_string()}]},
            {"op":"remove","path":format!("members[value eq \"{a}\"]")},
            {"op":"Remove","path":"members","value":[{"value":c.to_string()}]},
            {"op":"replace","value":{"id":"ignored","displayName":"Eng","externalId":"grp-1"}}
        ]}),
    )
    .unwrap();
    assert_eq!(
        group,
        GroupDraft {
            display_name: "Eng".into(),
            external_id: Some("grp-1".into()),
            members: BTreeSet::from([b]),
        }
    );
    apply_group_patch(
        &mut group,
        &json!({"Operations":[{"op":"replace","path":"members","value":[{"value":a.to_string()}]}]}),
    )
    .unwrap();
    assert_eq!(group.members, BTreeSet::from([a]));
    apply_group_patch(
        &mut group,
        &json!({"Operations":[{"op":"remove","path":"members"}]}),
    )
    .unwrap();
    assert!(group.members.is_empty());
    for body in [
        json!({"Operations":[{"op":"add","path":"members","value":[{"value":"not-a-uuid"}]}]}),
        json!({"Operations":[{"op":"remove","path":"displayName"}]}),
        json!({"Operations":[{"op":"remove","path":"members[display eq \"x\"]"}]}),
        json!({"Operations":[{"op":"replace","path":"displayName","value":""}]}),
    ] {
        assert!(
            apply_group_patch(&mut group.clone(), &body).is_err(),
            "{body}"
        );
    }
}

#[cfg(feature = "integration-tests")]
mod database {
    use super::*;
    use sqlx::PgPool;

    async fn person(pool: &PgPool, email: &str, manual: Option<&str>) -> Uuid {
        let id = Uuid::new_v4();
        sqlx::query("INSERT INTO users(id,email) VALUES($1,$2)")
            .bind(id)
            .bind(email)
            .execute(pool)
            .await
            .unwrap();
        if let Some(role) = manual {
            sqlx::query(
                "INSERT INTO platform_role_grants(user_id,role,source) VALUES($1,$2,'manual')",
            )
            .bind(id)
            .bind(role)
            .execute(pool)
            .await
            .unwrap();
        }
        id
    }

    async fn workspace(pool: &PgPool, kind: &str, owner: Option<Uuid>) -> Uuid {
        let id = Uuid::new_v4();
        sqlx::query("INSERT INTO workspaces(id,name,kind,owner_user_id) VALUES($1,$2,$3,$4)")
            .bind(id)
            .bind(format!("{kind}-{id}"))
            .bind(kind)
            .bind(owner)
            .execute(pool)
            .await
            .unwrap();
        id
    }

    async fn member(pool: &PgPool, ws: Uuid, user: Uuid, role: &str) {
        sqlx::query("INSERT INTO workspace_membership_grants(workspace_id,user_id,role,source) VALUES($1,$2,$3,'manual')")
            .bind(ws).bind(user).bind(role).execute(pool).await.unwrap();
    }

    async fn key(pool: &PgPool, ws: Uuid, user: Uuid) -> Uuid {
        let id = Uuid::new_v4();
        let mut secret = [0u8; 32];
        secret[..16].copy_from_slice(id.as_bytes());
        sqlx::query("INSERT INTO api_keys(id,workspace_id,issued_to_user_id,name,secret_hash) VALUES($1,$2,$3,'k',$4)")
            .bind(id).bind(ws).bind(user).bind(secret.to_vec()).execute(pool).await.unwrap();
        id
    }

    async fn session(pool: &PgPool, user: Uuid) {
        let mut hash = [0u8; 32];
        hash[..16].copy_from_slice(Uuid::new_v4().as_bytes());
        sqlx::query("INSERT INTO browser_sessions(token_hash,user_id,csrf_hash,expires_at,verified_email) VALUES($1,$2,$1,now()+interval '1 hour','x@example.test')")
            .bind(hash.to_vec()).bind(user).execute(pool).await.unwrap();
    }

    async fn mapping_platform(pool: &PgPool, issuer: &str, group: &str, role: &str) -> Uuid {
        let id = Uuid::new_v4();
        sqlx::query("INSERT INTO oidc_group_mappings(id,issuer,group_value,target_kind,platform_role) VALUES($1,$2,$3,'platform',$4)")
            .bind(id).bind(issuer).bind(group).bind(role).execute(pool).await.unwrap();
        id
    }

    async fn mapping_workspace(pool: &PgPool, group: &str, ws: Uuid) -> Uuid {
        let id = Uuid::new_v4();
        sqlx::query("INSERT INTO oidc_group_mappings(id,issuer,group_value,target_kind,workspace_id,workspace_role) VALUES($1,$2,$3,'workspace',$4,'member')")
            .bind(id).bind(ISSUER).bind(group).bind(ws).execute(pool).await.unwrap();
        id
    }

    async fn revoked(pool: &PgPool, key: Uuid) -> bool {
        sqlx::query_scalar("SELECT revoked_at IS NOT NULL FROM api_keys WHERE id=$1")
            .bind(key)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    async fn state(pool: &PgPool, user: Uuid) -> (bool, Option<String>) {
        sqlx::query_as("SELECT disabled_at IS NOT NULL,disable_reason FROM users WHERE id=$1")
            .bind(user)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    /// (source, role) of active platform grants.
    async fn platform_grants(pool: &PgPool, user: Uuid) -> Vec<(String, String)> {
        sqlx::query_as("SELECT source,role FROM platform_role_grants WHERE user_id=$1 AND revoked_at IS NULL ORDER BY source,role")
            .bind(user).fetch_all(pool).await.unwrap()
    }

    async fn workspace_grants(pool: &PgPool, user: Uuid, ws: Uuid) -> Vec<String> {
        sqlx::query_scalar("SELECT source FROM workspace_membership_grants WHERE user_id=$1 AND workspace_id=$2 AND revoked_at IS NULL ORDER BY source")
            .bind(user).bind(ws).fetch_all(pool).await.unwrap()
    }

    async fn sessions_open(pool: &PgPool, user: Uuid) -> i64 {
        sqlx::query_scalar(
            "SELECT count(*) FROM browser_sessions WHERE user_id=$1 AND revoked_at IS NULL",
        )
        .bind(user)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    fn patch(ops: Value) -> Value {
        json!({"schemas":[PATCH_OP],"Operations":ops})
    }

    #[sqlx::test(migrations = "./enterprise_migrations")]
    async fn users_are_provisioned_filtered_and_replaced(pool: PgPool) {
        let app = app(pool.clone(), true);
        let auth = bearer();
        let existing = person(&pool, "bob@example.com", Some("user")).await;
        let (status, headers, user) = send(&app, Method::POST, "/scim/v2/Users", Some(&auth), Some(json!({
            "schemas":[USER_SCHEMA],"userName":"Ada@Example.com","externalId":"00u-ada",
            "name":{"givenName":"Ada","familyName":"Lovelace"},"displayName":"Ada Lovelace",
            "emails":[{"value":"ada.work@example.com","type":"work","primary":true},{"value":"ada@home.test"}],
            "active":true,"title":"ignored"
        }))).await;
        assert_eq!(status, StatusCode::CREATED, "{user}");
        let id: Uuid = user["id"].as_str().unwrap().parse().unwrap();
        assert_eq!(
            headers[header::LOCATION],
            format!("{BASE}/Users/{id}").as_str()
        );
        assert_eq!(headers[header::CONTENT_TYPE], "application/scim+json");
        assert_eq!(user["userName"], "Ada@Example.com");
        assert_eq!(user["emails"][0]["value"], "ada.work@example.com");
        assert_eq!(user["name"]["formatted"], "Ada Lovelace");
        assert_eq!(user["meta"]["location"], format!("{BASE}/Users/{id}"));
        // Provisioning grants nothing and pre-authorizes one-time OIDC linking only.
        let row: (bool, bool, Option<String>) = sqlx::query_as(
            "SELECT oidc_link_allowed,disabled_at IS NULL,display_name FROM users WHERE id=$1",
        )
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(row, (true, true, Some("Ada Lovelace".into())));
        assert!(platform_grants(&pool, id).await.is_empty());

        // Okta: filter userName eq (case-insensitive), matching unlinked users by email too.
        let (_, _, found) = send(
            &app,
            Method::GET,
            "/scim/v2/Users?filter=userName%20eq%20%22ada%40example.com%22",
            Some(&auth),
            None,
        )
        .await;
        assert_eq!(found["totalResults"], 1);
        assert_eq!(found["Resources"][0]["id"], id.to_string());
        let (_, _, bob) = send(
            &app,
            Method::GET,
            "/scim/v2/Users?filter=userName+eq+%22BOB%40example.com%22",
            Some(&auth),
            None,
        )
        .await;
        assert_eq!(bob["Resources"][0]["id"], existing.to_string());
        assert_eq!(bob["Resources"][0]["active"], true);
        let (_, _, by_external) = send(
            &app,
            Method::GET,
            "/scim/v2/Users?filter=externalId%20eq%20%2200u-ada%22",
            Some(&auth),
            None,
        )
        .await;
        assert_eq!(by_external["totalResults"], 1);
        let (_, _, none) = send(
            &app,
            Method::GET,
            "/scim/v2/Users?filter=userName%20eq%20%22nobody%40example.com%22",
            Some(&auth),
            None,
        )
        .await;
        assert_eq!(
            (none["totalResults"].clone(), none["Resources"].clone()),
            (json!(0), json!([]))
        );
        let (status, _, bad) = send(
            &app,
            Method::GET,
            "/scim/v2/Users?filter=userName%20sw%20%22a%22",
            Some(&auth),
            None,
        )
        .await;
        assert_eq!(
            (status, bad["scimType"].clone()),
            (StatusCode::BAD_REQUEST, json!("invalidFilter"))
        );
        // Pagination.
        let (_, _, page) = send(
            &app,
            Method::GET,
            "/scim/v2/Users?startIndex=2&count=1",
            Some(&auth),
            None,
        )
        .await;
        assert_eq!(
            (
                page["totalResults"].clone(),
                page["startIndex"].clone(),
                page["itemsPerPage"].clone()
            ),
            (json!(2), json!(2), json!(1))
        );

        // Uniqueness: userName or email already in use.
        for body in [
            json!({"userName":"ada@example.com","emails":[{"value":"other@example.com"}]}),
            json!({"userName":"carol@example.com","emails":[{"value":"BOB@example.com"}]}),
            json!({"userName":"bob@example.com"}),
        ] {
            let (status, _, error) = send(
                &app,
                Method::POST,
                "/scim/v2/Users",
                Some(&auth),
                Some(body),
            )
            .await;
            assert_eq!(
                (status, error["scimType"].clone()),
                (StatusCode::CONFLICT, json!("uniqueness"))
            );
        }
        for body in [
            json!({"emails":[{"value":"x@example.com"}]}),
            json!({"userName":"no-email"}),
            json!([1]),
        ] {
            let (status, _, _) = send(
                &app,
                Method::POST,
                "/scim/v2/Users",
                Some(&auth),
                Some(body),
            )
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST);
        }

        // PUT replaces optional attributes; absent displayName is kept.
        session(&pool, id).await;
        let (status, _, replaced) = send(&app, Method::PUT, &format!("/scim/v2/Users/{id}"), Some(&auth), Some(json!({
            "schemas":[USER_SCHEMA],"userName":"ada@example.com","emails":[{"value":"ada@example.com","primary":true}],"active":true
        }))).await;
        assert_eq!(status, StatusCode::OK, "{replaced}");
        assert!(replaced.get("externalId").is_none() && replaced.get("name").is_none());
        assert_eq!(replaced["displayName"], "Ada Lovelace");
        // Email change is not fresh OIDC proof: sessions end.
        assert_eq!(sessions_open(&pool, id).await, 0);
        // Taking someone else's email conflicts.
        let (status, _, _) = send(
            &app,
            Method::PUT,
            &format!("/scim/v2/Users/{id}"),
            Some(&auth),
            Some(json!({"userName":"ada@example.com","emails":[{"value":"bob@example.com"}]})),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        for path in [
            "/scim/v2/Users/not-a-uuid",
            "/scim/v2/Users/00000000-0000-0000-0000-000000000000",
        ] {
            let (status, _, _) = send(&app, Method::GET, path, Some(&auth), None).await;
            assert_eq!(status, StatusCode::NOT_FOUND);
            let (status, _, _) = send(
                &app,
                Method::PATCH,
                path,
                Some(&auth),
                Some(patch(json!([{"op":"replace","value":{"active":false}}]))),
            )
            .await;
            assert_eq!(status, StatusCode::NOT_FOUND);
        }
        let last: Option<DateTime<Utc>> =
            sqlx::query_scalar("SELECT last_write_at FROM scim_state")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(last.is_some());
        let audit: i64 = sqlx::query_scalar("SELECT count(*) FROM audit_events WHERE action IN ('scim.user.created','scim.user.updated') AND actor_user_id IS NULL").fetch_one(&pool).await.unwrap();
        assert_eq!(audit, 2);
    }

    #[sqlx::test(migrations = "./enterprise_migrations")]
    async fn deactivation_suspends_and_reactivation_never_restores_keys(pool: PgPool) {
        let app = app(pool.clone(), true);
        let auth = bearer();
        let user = person(&pool, "dana@example.com", Some("user")).await;
        let personal = workspace(&pool, "personal", Some(user)).await;
        member(&pool, personal, user, "owner").await;
        let project = workspace(&pool, "project", None).await;
        member(&pool, project, user, "member").await;
        let personal_key = key(&pool, personal, user).await;
        let project_key = key(&pool, project, user).await;
        session(&pool, user).await;
        let uri = format!("/scim/v2/Users/{user}");

        // Okta-style deactivation.
        let (status, _, body) = send(
            &app,
            Method::PATCH,
            &uri,
            Some(&auth),
            Some(patch(json!([{"op":"replace","value":{"active":false}}]))),
        )
        .await;
        assert_eq!(
            (status, body["active"].clone()),
            (StatusCode::OK, json!(false))
        );
        assert_eq!(
            state(&pool, user).await,
            (true, Some("scim_deactivated".into()))
        );
        assert!(revoked(&pool, personal_key).await && revoked(&pool, project_key).await);
        assert_eq!(sessions_open(&pool, user).await, 0);
        let cleanup_due: bool =
            sqlx::query_scalar("SELECT cleanup_due_at>now() FROM users WHERE id=$1")
                .bind(user)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(cleanup_due);
        // Manual grants are untouched; access is gone because the account is suspended.
        assert_eq!(
            platform_grants(&pool, user).await,
            [("manual".into(), "user".into())]
        );
        let effective: i64 =
            sqlx::query_scalar("SELECT count(*) FROM effective_platform_roles WHERE user_id=$1")
                .bind(user)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(effective, 0);

        // Entra-style reactivation: account returns, credentials do not.
        let (status, _, body) = send(
            &app,
            Method::PATCH,
            &uri,
            Some(&auth),
            Some(patch(
                json!([{"op":"Replace","path":"active","value":"True"}]),
            )),
        )
        .await;
        assert_eq!(
            (status, body["active"].clone()),
            (StatusCode::OK, json!(true))
        );
        assert_eq!(state(&pool, user).await, (false, None));
        assert!(revoked(&pool, personal_key).await && revoked(&pool, project_key).await);
        assert_eq!(sessions_open(&pool, user).await, 0);
        let new_key = key(&pool, personal, user).await;

        // DELETE deactivates (never deletes) and is idempotent.
        for _ in 0..2 {
            let (status, _, _) = send(&app, Method::DELETE, &uri, Some(&auth), None).await;
            assert_eq!(status, StatusCode::NO_CONTENT);
        }
        assert!(revoked(&pool, new_key).await);
        let (status, _, body) = send(&app, Method::GET, &uri, Some(&auth), None).await;
        assert_eq!(
            (status, body["active"].clone()),
            (StatusCode::OK, json!(false))
        );

        // SCIM never lifts an administrative suspension.
        let (status, _, _) = send(
            &app,
            Method::PATCH,
            &uri,
            Some(&auth),
            Some(patch(
                json!([{"op":"replace","path":"active","value":true}]),
            )),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        sqlx::query(
            "UPDATE users SET disabled_at=now(),disable_reason='admin_suspension' WHERE id=$1",
        )
        .bind(user)
        .execute(&pool)
        .await
        .unwrap();
        for active in [false, true] {
            send(
                &app,
                Method::PATCH,
                &uri,
                Some(&auth),
                Some(patch(
                    json!([{"op":"replace","path":"active","value":active}]),
                )),
            )
            .await;
            assert_eq!(
                state(&pool, user).await,
                (true, Some("admin_suspension".into()))
            );
        }
        // Expired grace: reactivation does not lift; cleanup tombstones and hides the user.
        sqlx::query("UPDATE users SET disable_reason='scim_deactivated',cleanup_due_at=now()-interval '1 second' WHERE id=$1").bind(user).execute(&pool).await.unwrap();
        send(
            &app,
            Method::PATCH,
            &uri,
            Some(&auth),
            Some(patch(
                json!([{"op":"replace","path":"active","value":false}]),
            )),
        )
        .await;
        send(
            &app,
            Method::PATCH,
            &uri,
            Some(&auth),
            Some(patch(
                json!([{"op":"replace","path":"active","value":true}]),
            )),
        )
        .await;
        assert_eq!(
            state(&pool, user).await,
            (true, Some("scim_deactivated".into()))
        );
        let store = Store::new(pool.clone());
        assert_eq!(
            crate::lifecycle::cleanup_inactive_accounts(&store)
                .await
                .unwrap(),
            1
        );
        let (status, _, _) = send(&app, Method::GET, &uri, Some(&auth), None).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (_, _, list) = send(&app, Method::GET, "/scim/v2/Users", Some(&auth), None).await;
        assert_eq!(list["totalResults"], 0);
        let pii: (Option<String>, Option<String>, Option<String>, bool) = sqlx::query_as(
            "SELECT user_name,external_id,given_name,active FROM scim_users WHERE user_id=$1",
        )
        .bind(user)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(pii, (None, None, None, false));
    }

    #[sqlx::test(migrations = "./enterprise_migrations")]
    async fn groups_feed_mappings_with_group_provenance(pool: PgPool) {
        let app = app(pool.clone(), true);
        let auth = bearer();
        let project = workspace(&pool, "project", None).await;
        mapping_platform(&pool, ISSUER, "Engineering", "user").await;
        mapping_platform(&pool, "https://other-issuer.test", "Engineering", "admin").await;
        mapping_workspace(&pool, "ext-proj", project).await;
        let manual = person(&pool, "manual@example.com", Some("user")).await;
        let (_, _, created) = send(
            &app,
            Method::POST,
            "/scim/v2/Users",
            Some(&auth),
            Some(json!({"userName":"ann@example.com"})),
        )
        .await;
        let ann: Uuid = created["id"].as_str().unwrap().parse().unwrap();

        // Members referencing unknown users are rejected, not dropped.
        let (status, _, _) = send(&app, Method::POST, "/scim/v2/Groups", Some(&auth), Some(json!({"displayName":"Engineering","members":[{"value":Uuid::new_v4().to_string()}]}))).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, headers, eng) = send(&app, Method::POST, "/scim/v2/Groups", Some(&auth), Some(json!({
            "schemas":[GROUP_SCHEMA],"displayName":"Engineering","members":[{"value":ann.to_string()},{"value":manual.to_string()}]
        }))).await;
        assert_eq!(status, StatusCode::CREATED, "{eng}");
        let eng_id = eng["id"].as_str().unwrap().to_owned();
        assert!(
            headers[header::LOCATION]
                .to_str()
                .unwrap()
                .ends_with(&eng_id)
        );
        assert_eq!(eng["members"].as_array().unwrap().len(), 2);
        assert_eq!(
            eng["members"][0]
                .as_object()
                .unwrap()
                .keys()
                .cloned()
                .collect::<BTreeSet<_>>(),
            BTreeSet::from(["$ref".to_owned(), "type".to_owned(), "value".to_owned()])
        );
        let (status, _, _) = send(
            &app,
            Method::POST,
            "/scim/v2/Groups",
            Some(&auth),
            Some(json!({"displayName":"Engineering"})),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        // Group provenance only for the configured issuer; manual grants stay separate.
        assert_eq!(
            platform_grants(&pool, ann).await,
            [("group".into(), "user".into())]
        );
        assert_eq!(
            platform_grants(&pool, manual).await,
            [
                ("group".into(), "user".into()),
                ("manual".into(), "user".into())
            ]
        );
        let (_, _, proj) = send(&app, Method::POST, "/scim/v2/Groups", Some(&auth), Some(json!({"displayName":"Project people","externalId":"ext-proj","members":[{"value":ann.to_string()}]}))).await;
        let proj_id = proj["id"].as_str().unwrap().to_owned();
        assert_eq!(workspace_grants(&pool, ann, project).await, ["group"]);
        let ann_key = key(&pool, project, ann).await;

        // Filters and excludedAttributes (Entra).
        let (_, _, found) = send(&app, Method::GET, "/scim/v2/Groups?filter=displayName%20eq%20%22engineering%22&excludedAttributes=members", Some(&auth), None).await;
        assert_eq!(found["totalResults"], 1);
        assert!(found["Resources"][0].get("members").is_none());
        let (_, _, by_ext) = send(
            &app,
            Method::GET,
            "/scim/v2/Groups?filter=externalId%20eq%20%22ext-proj%22",
            Some(&auth),
            None,
        )
        .await;
        assert_eq!(by_ext["Resources"][0]["id"], proj_id);

        // Removing workspace-group membership revokes that grant and the user's keys there.
        let (status, _, body) = send(
            &app,
            Method::PATCH,
            &format!("/scim/v2/Groups/{proj_id}"),
            Some(&auth),
            Some(patch(
                json!([{"op":"remove","path":format!("members[value eq \"{ann}\"]")}]),
            )),
        )
        .await;
        assert_eq!((status, body), (StatusCode::NO_CONTENT, Value::Null));
        assert!(workspace_grants(&pool, ann, project).await.is_empty());
        assert!(revoked(&pool, ann_key).await);

        // Entra-style removal: the manual user keeps manual access.
        send(
            &app,
            Method::PATCH,
            &format!("/scim/v2/Groups/{eng_id}"),
            Some(&auth),
            Some(patch(
                json!([{"op":"Remove","path":"members","value":[{"value":manual.to_string()}]}]),
            )),
        )
        .await;
        assert_eq!(
            platform_grants(&pool, manual).await,
            [("manual".into(), "user".into())]
        );
        assert_eq!(state(&pool, manual).await, (false, None));

        // Losing the last grant is entitlement loss; regaining it within grace lifts only that.
        let personal = workspace(&pool, "personal", Some(ann)).await;
        let personal_key = key(&pool, personal, ann).await;
        session(&pool, ann).await;
        send(
            &app,
            Method::PATCH,
            &format!("/scim/v2/Groups/{eng_id}"),
            Some(&auth),
            Some(patch(
                json!([{"op":"remove","path":"members","value":[{"value":ann.to_string()}]}]),
            )),
        )
        .await;
        assert!(platform_grants(&pool, ann).await.is_empty());
        assert_eq!(
            state(&pool, ann).await,
            (true, Some("entitlement_loss".into()))
        );
        assert!(revoked(&pool, personal_key).await);
        assert_eq!(sessions_open(&pool, ann).await, 0);
        send(
            &app,
            Method::PATCH,
            &format!("/scim/v2/Groups/{eng_id}"),
            Some(&auth),
            Some(patch(
                json!([{"op":"add","path":"members","value":[{"value":ann.to_string()}]}]),
            )),
        )
        .await;
        assert_eq!(
            platform_grants(&pool, ann).await,
            [("group".into(), "user".into())]
        );
        assert_eq!(state(&pool, ann).await, (false, None));
        assert!(
            revoked(&pool, personal_key).await,
            "revoked keys never return"
        );
        // SCIM-deactivated users are not reactivated by group changes.
        send(
            &app,
            Method::PATCH,
            &format!("/scim/v2/Users/{ann}"),
            Some(&auth),
            Some(patch(
                json!([{"op":"replace","path":"active","value":false}]),
            )),
        )
        .await;
        send(
            &app,
            Method::PATCH,
            &format!("/scim/v2/Groups/{eng_id}"),
            Some(&auth),
            Some(patch(
                json!([{"op":"replace","path":"members","value":[{"value":manual.to_string()}]}]),
            )),
        )
        .await;
        send(
            &app,
            Method::PATCH,
            &format!("/scim/v2/Groups/{eng_id}"),
            Some(&auth),
            Some(patch(
                json!([{"op":"add","path":"members","value":[{"value":ann.to_string()}]}]),
            )),
        )
        .await;
        assert_eq!(
            state(&pool, ann).await,
            (true, Some("scim_deactivated".into()))
        );
        let synced: i64 = sqlx::query_scalar("SELECT count(*) FROM audit_events WHERE action='identity.groups_synchronized' AND metadata->>'source'='scim'").fetch_one(&pool).await.unwrap();
        assert!(synced >= 5);
    }

    #[sqlx::test(migrations = "./enterprise_migrations")]
    async fn scim_takes_over_pushed_groups_and_delete_or_rename_revokes(pool: PgPool) {
        let app = app(pool.clone(), true);
        let auth = bearer();
        let contractors = mapping_platform(&pool, ISSUER, "Contractors", "user").await;
        let ops = mapping_platform(&pool, ISSUER, "ops-ext", "auditor").await;
        // A token-derived grant (sign-in) for a group SCIM has not pushed yet.
        let carl = person(&pool, "carl@example.com", None).await;
        sqlx::query("INSERT INTO platform_role_grants(user_id,role,source,mapping_id) VALUES($1,'user','group',$2)").bind(carl).bind(contractors).execute(&pool).await.unwrap();
        let olga = person(&pool, "olga@example.com", Some("user")).await;
        // Pushing the group makes SCIM its authority: non-members lose its grant.
        let (status, _, _) = send(
            &app,
            Method::POST,
            "/scim/v2/Groups",
            Some(&auth),
            Some(json!({"displayName":"Contractors"})),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        assert!(platform_grants(&pool, carl).await.is_empty());
        assert_eq!(
            state(&pool, carl).await,
            (true, Some("entitlement_loss".into()))
        );

        // Matching by externalId (Entra object ids in the groups claim).
        let (_, _, group) = send(&app, Method::POST, "/scim/v2/Groups", Some(&auth), Some(json!({"displayName":"Ops","externalId":"ops-ext","members":[{"value":olga.to_string()}]}))).await;
        let id = group["id"].as_str().unwrap().to_owned();
        assert_eq!(
            platform_grants(&pool, olga).await,
            [
                ("group".into(), "auditor".into()),
                ("manual".into(), "user".into())
            ]
        );
        let mapping: Option<Uuid> = sqlx::query_scalar("SELECT mapping_id FROM platform_role_grants WHERE user_id=$1 AND source='group' AND revoked_at IS NULL").bind(olga).fetch_one(&pool).await.unwrap();
        assert_eq!(mapping, Some(ops));
        // Rename keeps the externalId match.
        let (status, _, renamed) = send(&app, Method::PUT, &format!("/scim/v2/Groups/{id}"), Some(&auth), Some(json!({"displayName":"Operations","externalId":"ops-ext","members":[{"value":olga.to_string()}]}))).await;
        assert_eq!(
            (status, renamed["displayName"].clone()),
            (StatusCode::OK, json!("Operations"))
        );
        assert_eq!(platform_grants(&pool, olga).await.len(), 2);
        // Changing the externalId away from the mapping revokes the derived grant.
        send(
            &app,
            Method::PATCH,
            &format!("/scim/v2/Groups/{id}"),
            Some(&auth),
            Some(patch(
                json!([{"op":"replace","path":"externalId","value":"other"}]),
            )),
        )
        .await;
        assert_eq!(
            platform_grants(&pool, olga).await,
            [("manual".into(), "user".into())]
        );
        send(
            &app,
            Method::PATCH,
            &format!("/scim/v2/Groups/{id}"),
            Some(&auth),
            Some(patch(
                json!([{"op":"replace","value":{"externalId":"ops-ext"}}]),
            )),
        )
        .await;
        assert_eq!(platform_grants(&pool, olga).await.len(), 2);
        // Deleting the group revokes what it granted; manual access remains.
        let (status, _, _) = send(
            &app,
            Method::DELETE,
            &format!("/scim/v2/Groups/{id}"),
            Some(&auth),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        assert_eq!(
            platform_grants(&pool, olga).await,
            [("manual".into(), "user".into())]
        );
        let (status, _, _) = send(
            &app,
            Method::GET,
            &format!("/scim/v2/Groups/{id}"),
            Some(&auth),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let members: i64 = sqlx::query_scalar("SELECT count(*) FROM scim_group_members")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(members, 0);
    }

    #[sqlx::test(migrations = "./enterprise_migrations")]
    async fn resources_expose_directory_attributes_only(pool: PgPool) {
        let app = app(pool.clone(), true);
        let auth = bearer();
        let admin = person(&pool, "root@example.com", Some("admin")).await;
        let personal = workspace(&pool, "personal", Some(admin)).await;
        key(&pool, personal, admin).await;
        let (_, _, user) = send(
            &app,
            Method::GET,
            &format!("/scim/v2/Users/{admin}"),
            Some(&auth),
            None,
        )
        .await;
        let keys: BTreeSet<String> = user.as_object().unwrap().keys().cloned().collect();
        assert_eq!(
            keys,
            BTreeSet::from(
                ["schemas", "id", "userName", "active", "emails", "meta"].map(String::from)
            )
        );
        let text = user.to_string();
        for leaked in ["admin\"", "workspace", "key", "role", TOKEN] {
            assert!(!text.contains(leaked), "{leaked} in {text}");
        }
        let mut settings = pool.begin().await.unwrap();
        let status = summary(&mut settings, Some(&runtime())).await.unwrap();
        assert_eq!(status["base_url"], BASE);
        assert_eq!(
            (status["users"].clone(), status["groups"].clone()),
            (json!(0), json!(0))
        );
        assert!(!status.to_string().contains(TOKEN));
        assert_eq!(
            summary(&mut settings, None).await.unwrap(),
            json!({"enabled": false})
        );
    }

    async fn refused(app: &Router, method: Method, uri: &str, body: Option<Value>) {
        let (status, _, error) = send(app, method, uri, Some(&bearer()), body).await;
        assert_eq!(status, StatusCode::CONFLICT, "{error}");
        assert_eq!(
            (
                error["status"].clone(),
                error["scimType"].clone(),
                error["detail"].clone()
            ),
            (json!("409"), json!("mutability"), json!(LAST_ADMIN))
        );
        assert_eq!(error["schemas"], json!([ERROR_SCHEMA]));
    }

    async fn scalar(pool: &PgPool, sql: &str) -> i64 {
        sqlx::query_scalar(sql).fetch_one(pool).await.unwrap()
    }

    #[sqlx::test(migrations = "./enterprise_migrations")]
    async fn scim_never_removes_the_last_active_platform_admin(pool: PgPool) {
        let app = app(pool.clone(), true);
        let auth = bearer();
        let root = person(&pool, "root@example.com", Some("admin")).await;
        let personal = workspace(&pool, "personal", Some(root)).await;
        let root_key = key(&pool, personal, root).await;
        session(&pool, root).await;
        let uri = format!("/scim/v2/Users/{root}");
        // Deactivation in every form is refused whole: no suspension, no revoked
        // credentials, no partial attribute change.
        refused(
            &app,
            Method::PATCH,
            &uri,
            Some(patch(json!([
                {"op":"replace","path":"displayName","value":"Changed"},
                {"op":"replace","value":{"active":false}}
            ]))),
        )
        .await;
        refused(
            &app,
            Method::PUT,
            &uri,
            Some(json!({"userName":"root@example.com","displayName":"Changed","active":false})),
        )
        .await;
        refused(&app, Method::DELETE, &uri, None).await;
        assert_eq!(state(&pool, root).await, (false, None));
        assert!(!revoked(&pool, root_key).await);
        assert_eq!(sessions_open(&pool, root).await, 1);
        assert_eq!(
            platform_grants(&pool, root).await,
            [("manual".into(), "admin".into())]
        );
        let (_, _, body) = send(&app, Method::GET, &uri, Some(&auth), None).await;
        assert_eq!(body["active"], true);
        assert!(body.get("displayName").is_none(), "{body}");
        let scim_rows = scalar(&pool, "SELECT count(*) FROM scim_users").await;
        assert_eq!(scim_rows, 0, "nothing of the refused writes was stored");
        // Every refusal is audited (no names or emails); one alert stays open.
        assert_eq!(
            scalar(&pool, "SELECT count(*) FROM audit_events WHERE action='scim.last_admin_protected' AND actor_user_id IS NULL AND metadata='{}'").await,
            3
        );
        assert_eq!(
            scalar(&pool, "SELECT count(*) FROM audit_events WHERE action IN ('scim.user.updated','scim.user.deactivated')").await,
            0
        );
        let (summary, severity, workspace_id): (String, String, Option<Uuid>) = sqlx::query_as("SELECT summary,severity,workspace_id FROM alert_events WHERE builtin='scim_last_admin' AND kind='scim_last_admin' AND resolved_at IS NULL")
            .fetch_one(&pool).await.unwrap();
        assert_eq!(
            (summary.as_str(), severity.as_str(), workspace_id),
            (
                "SCIM tried to remove the last Platform Admin",
                "critical",
                None
            )
        );
        assert_eq!(
            scalar(&pool, "SELECT count(*) FROM alert_deliveries d JOIN alert_events e ON e.id=d.event_id WHERE e.builtin='scim_last_admin'").await,
            1
        );

        // Group provenance: Ann is an Admin only through a SCIM-pushed group.
        mapping_platform(&pool, ISSUER, "Admins", "admin").await;
        let ann = person(&pool, "ann@example.com", None).await;
        let (status, _, group) = send(
            &app,
            Method::POST,
            "/scim/v2/Groups",
            Some(&auth),
            Some(json!({
                "displayName":"Admins","members":[{"value":ann.to_string()}]
            })),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{group}");
        let group_uri = format!("/scim/v2/Groups/{}", group["id"].as_str().unwrap());
        assert_eq!(
            platform_grants(&pool, ann).await,
            [("group".into(), "admin".into())]
        );
        // With a second Admin, Root can be deactivated.
        let (status, _, _) = send(
            &app,
            Method::PATCH,
            &uri,
            Some(&auth),
            Some(patch(
                json!([{"op":"replace","path":"active","value":false}]),
            )),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(state(&pool, root).await.0);
        // Now Ann is the last Admin: no group change may drop her grant.
        for (method, body) in [
            (
                Method::PATCH,
                Some(patch(
                    json!([{"op":"remove","path":format!("members[value eq \"{ann}\"]")}]),
                )),
            ),
            (
                Method::PATCH,
                Some(patch(
                    json!([{"op":"replace","path":"displayName","value":"Former admins"}]),
                )),
            ),
            (
                Method::PUT,
                Some(json!({"displayName":"Admins","members":[]})),
            ),
            (Method::DELETE, None),
        ] {
            refused(&app, method, &group_uri, body).await;
        }
        refused(&app, Method::DELETE, &format!("/scim/v2/Users/{ann}"), None).await;
        assert_eq!(
            platform_grants(&pool, ann).await,
            [("group".into(), "admin".into())]
        );
        assert_eq!(state(&pool, ann).await, (false, None));
        assert_eq!(
            scalar(&pool, "SELECT count(*) FROM scim_group_members").await,
            1
        );
        assert_eq!(
            scalar(
                &pool,
                "SELECT count(*) FROM alert_events WHERE builtin='scim_last_admin'"
            )
            .await,
            1,
            "repeated refusals keep one open incident"
        );
        // A second active Admin clears the incident and allows the change.
        person(&pool, "bob@example.com", Some("admin")).await;
        crate::alerts::evaluate_once(&Store::new(pool.clone()))
            .await
            .unwrap();
        assert_eq!(
            scalar(&pool, "SELECT count(*) FROM alert_events WHERE builtin='scim_last_admin' AND resolution='cleared'").await,
            1
        );
        let (status, _, _) = send(
            &app,
            Method::PATCH,
            &group_uri,
            Some(&auth),
            Some(patch(json!([{"op":"remove","path":"members"}]))),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        assert!(platform_grants(&pool, ann).await.is_empty());
    }

    #[sqlx::test(migrations = "./enterprise_migrations")]
    async fn concurrent_deactivations_keep_one_admin(pool: PgPool) {
        let app = app(pool.clone(), true);
        let a = person(&pool, "a@example.com", Some("admin")).await;
        let b = person(&pool, "b@example.com", Some("admin")).await;
        let body = || {
            Some(patch(
                json!([{"op":"replace","path":"active","value":false}]),
            ))
        };
        let auth = bearer();
        let (ua, ub) = (format!("/scim/v2/Users/{a}"), format!("/scim/v2/Users/{b}"));
        let ((sa, _, _), (sb, _, _)) = tokio::join!(
            send(&app, Method::PATCH, &ua, Some(&auth), body()),
            send(&app, Method::PATCH, &ub, Some(&auth), body()),
        );
        let mut statuses = [sa, sb];
        statuses.sort();
        assert_eq!(statuses, [StatusCode::OK, StatusCode::CONFLICT]);
        assert_eq!(
            scalar(
                &pool,
                "SELECT count(*) FROM effective_platform_roles WHERE role='admin'"
            )
            .await,
            1
        );
    }
}
