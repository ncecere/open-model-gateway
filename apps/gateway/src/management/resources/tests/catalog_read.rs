use super::*;

async fn catalog_rows(pool: &PgPool) -> [(String, Uuid, Value); 3] {
    let provider = Uuid::new_v4();
    let model = Uuid::new_v4();
    let deployment = Uuid::new_v4();
    sqlx::query("INSERT INTO provider_connections(id,name,provider,credential_ref,region,enabled) VALUES($1,'zz Needle%_','bedrock','aws:private-sentinel','us-east-1',false)")
        .bind(provider).execute(pool).await.unwrap();
    sqlx::query("INSERT INTO models(id,public_name,display_name,enabled) VALUES($1,'zz/Needle%_','Display sentinel',false)")
        .bind(model).execute(pool).await.unwrap();
    sqlx::query("INSERT INTO deployments(id,model_id,provider_connection_id,upstream_model,enabled,created_at) VALUES($1,$2,$3,'zz Needle%_',false,'2100-01-01')")
        .bind(deployment).bind(model).bind(provider).execute(pool).await.unwrap();
    [
        (
            "providers".into(),
            provider,
            json!({"id":provider,"name":"zz Needle%_","provider":"bedrock","endpoint":null,"region":"us-east-1","enabled":false}),
        ),
        (
            "models".into(),
            model,
            json!({"id":model,"public_name":"zz/Needle%_","display_name":"Display sentinel","enabled":false}),
        ),
        (
            "deployments".into(),
            deployment,
            json!({"id":deployment,"model_id":model,"provider_connection_id":provider,"upstream_model":"zz Needle%_","enabled":false}),
        ),
    ]
}

#[sqlx::test]
async fn details_match_redacted_collections_and_require_live_operator(pool: PgPool) {
    let f = fixture(&pool).await;
    let rows = catalog_rows(&pool).await;
    let foreign = BrowserPrincipal {
        user_id: Uuid::new_v4(),
        email: "foreign-admin@example.invalid".into(),
        platform_admin: false,
    };
    let foreign_org = Uuid::new_v4();
    sqlx::query("INSERT INTO users(id,email) VALUES($1,$2)")
        .bind(foreign.user_id)
        .bind(&foreign.email)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO organizations(id,slug,name) VALUES($1,'foreign','Foreign')")
        .bind(foreign_org)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO organization_memberships(organization_id,user_id,role) VALUES($1,$2,'owner')",
    )
    .bind(foreign_org)
    .bind(foreign.user_id)
    .execute(&pool)
    .await
    .unwrap();
    let identity = crate::identity::IdentityState::new(f.store.clone(), None)
        .await
        .unwrap();
    let authenticated = super::super::super::router(identity).with_state(f.store.clone());
    for (resource, id, expected) in &rows {
        let collection = format!("/api/v1/platform/{resource}");
        let detail = format!("{collection}/{id}");
        let missing = format!("{collection}/{}", Uuid::new_v4());
        assert_eq!(
            call(&f, &f.operator, "GET", &detail, json!({})).await,
            (StatusCode::OK, expected.clone())
        );
        let (status, list) = call(&f, &f.operator, "GET", &collection, json!({})).await;
        assert_eq!(status, StatusCode::OK);
        assert!(list["data"].as_array().unwrap().contains(expected));
        assert!(!list.to_string().contains("credential_ref"));
        assert!(!list.to_string().contains("private-sentinel"));
        assert_eq!(
            call(&f, &f.operator, "GET", &missing, json!({})).await.0,
            StatusCode::NOT_FOUND
        );
        for path in [&collection, &detail, &missing] {
            for caller in [&f.admin, &foreign] {
                assert_eq!(
                    call(&f, caller, "GET", path, json!({})).await.0,
                    StatusCode::FORBIDDEN
                );
            }
            for token in [None, Some(&f.personal_token), Some(&f.team_token)] {
                let mut request = Request::builder().uri(path);
                if let Some(token) = token {
                    request = request.header("authorization", format!("Bearer {token}"));
                }
                let response = authenticated
                    .clone()
                    .oneshot(request.body(Body::empty()).unwrap())
                    .await
                    .unwrap();
                assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
            }
        }
    }
    // The injected principal deliberately remains stale after each live change.
    for update in [
        "UPDATE users SET platform_admin=false WHERE id=$1",
        "UPDATE users SET platform_admin=true,disabled_at=now() WHERE id=$1",
    ] {
        sqlx::query(update)
            .bind(f.operator.user_id)
            .execute(&pool)
            .await
            .unwrap();
        for (resource, id, _) in &rows {
            for path in [
                format!("/api/v1/platform/{resource}?q=needle"),
                format!("/api/v1/platform/{resource}/{id}"),
            ] {
                assert_eq!(
                    call(&f, &f.operator, "GET", &path, json!({})).await.0,
                    StatusCode::FORBIDDEN
                );
            }
        }
    }
}

#[sqlx::test]
async fn catalog_search_filters_before_paging_and_treats_patterns_literally(pool: PgPool) {
    let f = fixture(&pool).await;
    let rows = catalog_rows(&pool).await;
    let provider = rows[0].1;
    let model = rows[1].1;
    // More than the maximum page size, all sorting before the searched target.
    sqlx::query("INSERT INTO provider_connections(id,name,provider,credential_ref,enabled) SELECT gen_random_uuid(),'aa provider '||lpad(n::text,3,'0'),'openai','env:PRIVATE_SENTINEL',true FROM generate_series(1,205) n")
        .execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO models(id,public_name,display_name,enabled) SELECT gen_random_uuid(),'aa/model/'||lpad(n::text,3,'0'),'Model '||n,true FROM generate_series(1,205) n")
        .execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO deployments(id,model_id,provider_connection_id,upstream_model,enabled,created_at) SELECT gen_random_uuid(),$1,$2,'aa upstream '||n,true,'2000-01-01'::timestamptz + n*interval '1 second' FROM generate_series(1,205) n")
        .bind(model).bind(provider).execute(&pool).await.unwrap();
    for (resource, _, expected) in &rows {
        let base = format!("/api/v1/platform/{resource}");
        let (status, first) = call(&f, &f.operator, "GET", &base, json!({})).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(first["data"].as_array().unwrap().len(), 100);
        assert!(!first["data"].as_array().unwrap().contains(expected));
        let (_, max_page) = call(
            &f,
            &f.operator,
            "GET",
            &format!("{base}?limit=200"),
            json!({}),
        )
        .await;
        assert_eq!(max_page["data"].as_array().unwrap().len(), 200);
        for query in [
            "q=%20nEeDlE%20",
            "q=%25",
            "q=_",
            "q=%25_",
            "enabled=false&q=needle",
            "limit=1&offset=0&q=needle",
        ] {
            let result = call(
                &f,
                &f.operator,
                "GET",
                &format!("{base}?{query}"),
                json!({}),
            )
            .await;
            assert_eq!(
                result,
                (StatusCode::OK, json!({"data":[expected]})),
                "{resource}: {query}"
            );
        }
        for query in [
            "q=needle&enabled=true",
            "q=needle&offset=1",
            "q=PRIVATE_SENTINEL",
            "q=private-sentinel",
            "offset=100000",
        ] {
            assert_eq!(
                call(
                    &f,
                    &f.operator,
                    "GET",
                    &format!("{base}?{query}"),
                    json!({})
                )
                .await,
                (StatusCode::OK, json!({"data":[]})),
                "{resource}: {query}"
            );
        }
        // Blank search inherits ordinary collection behavior, and offsets are stable.
        assert_eq!(
            call(
                &f,
                &f.operator,
                "GET",
                &format!("{base}?q=%20%20"),
                json!({})
            )
            .await
            .1,
            first
        );
        let second = call(
            &f,
            &f.operator,
            "GET",
            &format!("{base}?limit=1&offset=1"),
            json!({}),
        )
        .await;
        assert_eq!(second, (StatusCode::OK, json!({"data":[first["data"][1]]})));
        for query in [
            "limit=0".into(),
            "limit=201".into(),
            "offset=-1".into(),
            "offset=100001".into(),
            "enabled=yes".into(),
            "unknown=value".into(),
            format!("q={}", "x".repeat(201)),
        ] {
            assert_eq!(
                call(
                    &f,
                    &f.operator,
                    "GET",
                    &format!("{base}?{query}"),
                    json!({})
                )
                .await
                .0,
                StatusCode::BAD_REQUEST,
                "{resource}: {query}"
            );
        }
        assert_eq!(
            call(
                &f,
                &f.operator,
                "GET",
                &format!("{base}?q={}", "x".repeat(200)),
                json!({})
            )
            .await
            .0,
            StatusCode::OK
        );
        let (_, enabled) = call(
            &f,
            &f.operator,
            "GET",
            &format!("{base}?enabled=true"),
            json!({}),
        )
        .await;
        assert!(
            enabled["data"]
                .as_array()
                .unwrap()
                .iter()
                .all(|row| row["enabled"] == true)
        );
    }
    // Search covers safe human-readable fields, not credential references.
    for (resource, query, expected) in [
        ("providers", "q=us-east-1", &rows[0].2),
        ("models", "q=display%20sentinel", &rows[1].2),
    ] {
        assert_eq!(
            call(
                &f,
                &f.operator,
                "GET",
                &format!("/api/v1/platform/{resource}?{query}"),
                json!({})
            )
            .await,
            (StatusCode::OK, json!({"data":[expected]}))
        );
    }
    let base = "/api/v1/platform/deployments";
    for query in [
        format!("q=needle&model_id={model}"),
        format!("q=needle&provider_connection_id={provider}"),
        format!("enabled=false&model_id={model}&provider_connection_id={provider}"),
    ] {
        assert_eq!(
            call(
                &f,
                &f.operator,
                "GET",
                &format!("{base}?{query}"),
                json!({})
            )
            .await,
            (StatusCode::OK, json!({"data":[rows[2].2]}))
        );
    }
    for query in [
        format!("model_id={}", Uuid::new_v4()),
        format!("provider_connection_id={}", Uuid::new_v4()),
        format!("model_id={model}&provider_connection_id={}", Uuid::new_v4()),
    ] {
        assert_eq!(
            call(
                &f,
                &f.operator,
                "GET",
                &format!("{base}?{query}"),
                json!({})
            )
            .await,
            (StatusCode::OK, json!({"data":[]}))
        );
    }
    for field in ["model_id", "provider_connection_id"] {
        assert_eq!(
            call(
                &f,
                &f.operator,
                "GET",
                &format!("{base}?{field}=not-a-uuid"),
                json!({})
            )
            .await
            .0,
            StatusCode::BAD_REQUEST
        );
    }
}
