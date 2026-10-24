use super::*;

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn provider_cost_browser_only_origin_csrf_and_live_roles(pool: PgPool) {
    let f = fixture(&pool).await;
    let mock = MockProvider::start(None).await;
    let identity = mock.state(pool.clone()).await;
    let app = crate::management::router(identity).with_state(f.s.clone());
    let admin = session(&pool, &f.admin).await;
    let auditor = session(&pool, &f.auditor).await;
    let member = session(&pool, &f.owner).await;
    let (_, token) = call(&f.s, &f.admin, "POST", "/api/v1/platform/admin-tokens",
        json!({"name":"Cost refusal","scopes":["platform:read","governance:write"],"expires_in_days":1})).await;
    let token = token["token"].as_str().unwrap();
    for path in ["/api/v1/platform/provider-cost-imports", "/api/v1/platform/provider-cost-mappings", "/api/v1/platform/provider-reconciliations"] {
        assert_eq!(request(&app,"GET",path,Some(&admin),None,false,ORIGIN).await, StatusCode::OK);
        assert_eq!(request(&app,"GET",path,Some(&auditor),None,false,ORIGIN).await, StatusCode::OK);
        assert_eq!(request(&app,"GET",path,Some(&member),None,false,ORIGIN).await, StatusCode::FORBIDDEN);
        assert_eq!(request(&app,"POST",path,Some(&auditor),None,true,ORIGIN).await, StatusCode::FORBIDDEN);
        assert_eq!(request(&app,"POST",path,Some(&admin),None,false,ORIGIN).await, StatusCode::FORBIDDEN);
        assert_eq!(request(&app,"POST",path,Some(&admin),None,true,"https://attacker.invalid").await, StatusCode::FORBIDDEN);
        for method in ["GET","POST"] {
            assert_eq!(request(&app,method,path,None,Some(token),false,ORIGIN).await, StatusCode::FORBIDDEN);
        }
    }
    sqlx::query("UPDATE platform_role_grants SET revoked_at=now() WHERE user_id=$1").bind(f.admin.user_id).execute(&pool).await.unwrap();
    assert_eq!(request(&app,"POST","/api/v1/platform/provider-cost-imports",Some(&admin),None,true,ORIGIN).await, StatusCode::FORBIDDEN);
}
