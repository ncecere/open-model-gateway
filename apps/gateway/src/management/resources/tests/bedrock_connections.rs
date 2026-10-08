//! Bedrock connection management: identity modes, hidden references, endpoint and route
//! validation. The test process sets no GATEWAY_AWS_PROFILE_ALLOWLIST or
//! GATEWAY_BEDROCK_ENDPOINT_ALLOWLIST, so named profiles and endpoint overrides are refused
//! here; allowlisted acceptance is covered by `providers::bedrock` policy tests.
use super::*;

const ROLE: &str = "arn:aws:iam::123456789012:role/gateway/bedrock-invoke";

fn bedrock(credential_ref: &str) -> Value {
    json!({"name":"Bedrock","provider":"bedrock","credential_ref":credential_ref,"endpoint":null,"region":"us-east-1","enabled":false})
}
async fn stored(pool: &PgPool, id: Uuid) -> (String, Option<String>) {
    sqlx::query_as("SELECT credential_ref,endpoint FROM provider_connections WHERE id=$1")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn bedrock_identity_modes_are_validated_stored_canonically_and_never_returned(pool: PgPool) {
    let f = fixture(&pool).await;
    let post = |body: Value| call(&f.s, &f.admin, "POST", "/api/v1/platform/providers", body);
    let (status, v) = post(bedrock("aws:default")).await;
    assert_eq!(status, StatusCode::OK, "{v}");
    let default = id(&v);
    let mut body = bedrock(&format!("aws:role:{ROLE}"));
    body["aws_external_id"] = json!("tenant-ext-42");
    body["aws_session_name"] = json!("gateway.team-a");
    let (status, v) = post(body).await;
    assert_eq!(status, StatusCode::OK, "{v}");
    let role = id(&v);
    assert_eq!(
        stored(&pool, role).await.0,
        format!("aws:role:{ROLE};external_id=tenant-ext-42;session_name=gateway.team-a")
    );
    for (connection, mode) in [(default, "default"), (role, "role")] {
        for actor in [&f.admin, &f.auditor] {
            let (status, v) = call(
                &f.s,
                actor,
                "GET",
                &format!("/api/v1/platform/providers/{connection}"),
                json!({}),
            )
            .await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(v["aws_auth"], mode);
            assert_eq!(v["auth_mode"], "credential");
            assert!(v.get("credential_ref").is_none() && v.get("aws_external_id").is_none());
        }
    }
    let (_, list) = call(
        &f.s,
        &f.auditor,
        "GET",
        "/api/v1/platform/providers",
        json!({}),
    )
    .await;
    let text = list.to_string();
    assert!(
        !text.contains("tenant-ext-42")
            && !text.contains("123456789012")
            && !text.contains("gateway.team-a")
    );
    let mut bad = vec![
        bedrock("aws:profile:bedrock-prod"), // not on the (empty) profile allowlist
        bedrock("aws:role:not-an-arn"),
        bedrock(&format!("aws:role:{ROLE};external_id=inline")), // options are separate fields
        bedrock("env:AWS_SECRET_ACCESS_KEY"),
        bedrock("AKIAIOSFODNN7EXAMPLE"),
        json!({"name":"Bedrock","provider":"bedrock","credential_ref":"aws:default","endpoint":null,"region":"us-east","enabled":false}),
        json!({"name":"Bedrock","provider":"bedrock","credential_ref":"aws:default","endpoint":null,"region":null,"enabled":false}),
        json!({"name":"Bedrock","provider":"bedrock","credential_ref":"aws:default","endpoint":"https://vpce-0abc.bedrock-runtime.us-east-1.vpce.amazonaws.com","region":"us-east-1","enabled":false}),
        json!({"name":"Bedrock","provider":"bedrock","credential_ref":"aws:default","endpoint":"http://bedrock.internal","region":"us-east-1","enabled":false}),
        json!({"name":"Cloud","provider":"openai","credential_ref":"aws:default","enabled":false}),
        json!({"name":"Cloud","provider":"openai","credential_ref":"env:OPENAI_API_KEY","aws_session_name":"gw","enabled":false}),
    ];
    for (external_id, session_name) in [
        (json!("x"), json!(null)),
        (json!(null), json!("has space")),
        (json!("ok-ext"), json!(null)),
    ] {
        let mut body = bedrock(if external_id == json!("ok-ext") {
            "aws:default"
        } else {
            "aws:role:arn:aws:iam::123456789012:role/r"
        });
        body["aws_external_id"] = external_id;
        body["aws_session_name"] = session_name;
        bad.push(body);
    }
    for body in bad {
        assert_eq!(
            post(body.clone()).await.0,
            StatusCode::BAD_REQUEST,
            "{body}"
        );
    }
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM provider_connections")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 2);
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn bedrock_identity_replacement_and_endpoint_patch_are_bedrock_only(pool: PgPool) {
    let f = fixture(&pool).await;
    let (_, v) = call(
        &f.s,
        &f.admin,
        "POST",
        "/api/v1/platform/providers",
        bedrock("aws:default"),
    )
    .await;
    let connection = id(&v);
    let path = format!("/api/v1/platform/providers/{connection}");
    let patch = |body: Value| call(&f.s, &f.admin, "PATCH", &path, body);
    let (status, v) = patch(json!({"enabled":false,"credential_ref":format!("aws:role:{ROLE}"),"aws_session_name":"gw-rotated"})).await;
    assert_eq!(status, StatusCode::OK, "{v}");
    assert_eq!(
        stored(&pool, connection).await,
        (format!("aws:role:{ROLE};session_name=gw-rotated"), None)
    );
    // Status-only update keeps the identity; clearing the endpoint is explicit null.
    assert_eq!(patch(json!({"enabled":true})).await.0, StatusCode::OK);
    assert_eq!(
        patch(json!({"enabled":true,"endpoint":null})).await.0,
        StatusCode::OK
    );
    assert_eq!(
        stored(&pool, connection).await.0,
        format!("aws:role:{ROLE};session_name=gw-rotated")
    );
    for body in [
        json!({"enabled":true,"aws_external_id":"ext-without-role"}),
        json!({"enabled":true,"credential_ref":"aws:default","aws_external_id":"ext-1"}),
        json!({"enabled":true,"credential_ref":"aws:profile:not-allowlisted"}),
        json!({"enabled":true,"endpoint":"https://vpce-unlisted.example"}),
        json!({"enabled":true,"credential_ref":"env:OPENAI_API_KEY"}),
    ] {
        assert_eq!(
            patch(body.clone()).await.0,
            StatusCode::BAD_REQUEST,
            "{body}"
        );
    }
    assert_eq!(
        stored(&pool, connection).await.0,
        format!("aws:role:{ROLE};session_name=gw-rotated")
    );
    assert_eq!(
        call(&f.s, &f.auditor, "PATCH", &path, json!({"enabled":false}))
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    let cloud = Uuid::new_v4();
    sqlx::query("INSERT INTO provider_connections(id,name,provider,credential_ref) VALUES($1,'Cloud','openai','env:KEY')").bind(cloud).execute(&pool).await.unwrap();
    for body in [
        json!({"enabled":true,"endpoint":null}),
        json!({"enabled":true,"credential_ref":"aws:default"}),
    ] {
        assert_eq!(
            call(
                &f.s,
                &f.admin,
                "PATCH",
                &format!("/api/v1/platform/providers/{cloud}"),
                body
            )
            .await
            .0,
            StatusCode::BAD_REQUEST
        );
    }
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn bedrock_routes_accept_inference_profiles_and_same_region_arns_only(pool: PgPool) {
    let f = fixture(&pool).await;
    let m = model(&pool, "claude").await;
    let (_, v) = call(
        &f.s,
        &f.admin,
        "POST",
        "/api/v1/platform/providers",
        bedrock("aws:default"),
    )
    .await;
    let connection = id(&v);
    let route = |upstream: &str| {
        call(
            &f.s,
            &f.admin,
            "POST",
            "/api/v1/platform/deployments",
            json!({"model_id":m,"provider_connection_id":connection,"upstream_model":upstream,"enabled":false}),
        )
    };
    for upstream in [
        "anthropic.claude-3-5-sonnet-20240620-v1:0",
        "us.anthropic.claude-3-7-sonnet-20250219-v1:0",
        "global.anthropic.claude-sonnet-4-20250514-v1:0",
        "arn:aws:bedrock:us-east-1:123456789012:inference-profile/us.anthropic.claude-3-7-sonnet-20250219-v1:0",
        "arn:aws:bedrock:us-east-1:123456789012:application-inference-profile/a1b2c3d4e5",
    ] {
        let (status, v) = route(upstream).await;
        assert_eq!(status, StatusCode::OK, "{upstream}: {v}");
    }
    for upstream in [
        "arn:aws:bedrock:eu-west-1:123456789012:inference-profile/eu.anthropic.claude-3-7-sonnet-20250219-v1:0",
        "arn:aws:bedrock:us-east-1:123456789012:knowledge-base/kb1",
        "model with space",
        "us.anthropic.claude?x=1",
    ] {
        assert_eq!(
            route(upstream).await.0,
            StatusCode::BAD_REQUEST,
            "{upstream}"
        );
    }
    // Other profiles keep their own upstream naming rules.
    let cloud = Uuid::new_v4();
    sqlx::query("INSERT INTO provider_connections(id,name,provider,credential_ref) VALUES($1,'Router','openrouter','env:KEY')").bind(cloud).execute(&pool).await.unwrap();
    let (status, _) = call(&f.s, &f.admin, "POST", "/api/v1/platform/deployments", json!({"model_id":m,"provider_connection_id":cloud,"upstream_model":"openai/gpt-4o?free","enabled":false})).await;
    assert_eq!(status, StatusCode::OK);
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn database_constrains_aws_reference_shapes_and_bedrock_endpoints(pool: PgPool) {
    let insert = |provider: &'static str, reference: String, endpoint: Option<&'static str>| {
        let pool = pool.clone();
        async move {
            sqlx::query("INSERT INTO provider_connections(id,name,provider,credential_ref,endpoint,region) VALUES($1,'T',$2,$3,$4,'us-east-1')")
                .bind(Uuid::new_v4()).bind(provider).bind(reference).bind(endpoint).execute(&pool).await.is_ok()
        }
    };
    for (provider, reference, endpoint, ok) in [
        ("bedrock", "aws:default".to_owned(), None, true),
        (
            "bedrock",
            "aws:profile:bedrock-prod".to_owned(),
            Some("https://vpce-0abc.bedrock-runtime.us-east-1.vpce.amazonaws.com"),
            true,
        ),
        (
            "bedrock",
            format!("aws:role:{ROLE};external_id=a:b/c=d;session_name=gw"),
            None,
            true,
        ),
        (
            "bedrock",
            format!("aws:role:{ROLE};session_name=gw;external_id=wrong-order"),
            None,
            false,
        ),
        ("bedrock", "aws:role:not-an-arn".to_owned(), None, false),
        ("bedrock", "aws:profile:".to_owned(), None, false),
        ("bedrock", "env:KEY".to_owned(), None, false),
        (
            "bedrock",
            "aws:default".to_owned(),
            Some("http://bedrock.internal"),
            false,
        ),
        (
            "bedrock",
            "aws:default".to_owned(),
            Some("https://host.example/path"),
            false,
        ),
        ("openai", "aws:profile:bedrock-prod".to_owned(), None, false),
        ("openai", format!("aws:role:{ROLE}"), None, false),
    ] {
        assert_eq!(
            insert(provider, reference.clone(), endpoint).await,
            ok,
            "{provider} {reference} {endpoint:?}"
        );
    }
}
