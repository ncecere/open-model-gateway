//! Every JSON management write gets a successful control before the same
//! body plus an unknown configuration field. The source inventory below makes
//! newly added Json extractors fail until they have a real HTTP case here.
use super::*;
use std::collections::BTreeSet;
use syn::visit::{self, Visit};

fn json_handlers() -> BTreeSet<String> {
    struct Handlers {
        module: String,
        names: BTreeSet<String>,
    }
    impl<'ast> Visit<'ast> for Handlers {
        fn visit_item_mod(&mut self, node: &'ast syn::ItemMod) {
            if !crate::configuration_source_tests::test_only(&node.attrs) {
                visit::visit_item_mod(self, node);
            }
        }
        fn visit_item_fn(&mut self, node: &'ast syn::ItemFn) {
            if crate::configuration_source_tests::test_only(&node.attrs) {
                return;
            }
            let json = node.sig.inputs.iter().any(|arg| match arg {
                syn::FnArg::Typed(p) => match p.ty.as_ref() {
                    syn::Type::Path(p) => p.path.segments.last().is_some_and(|s| s.ident == "Json"),
                    _ => false,
                },
                _ => false,
            });
            if json {
                self.names
                    .insert(format!("{}::{}", self.module, node.sig.ident));
            }
            visit::visit_item_fn(self, node);
        }
    }
    let mut names = BTreeSet::new();
    for source in crate::configuration_source_tests::production_sources() {
        let Some(module) = source.name.strip_prefix("management/") else {
            continue;
        };
        let mut visitor = Handlers {
            module: module.trim_end_matches(".rs").replace('/', "::"),
            names: BTreeSet::new(),
        };
        visitor.visit_file(&source.ast);
        names.extend(visitor.names);
    }
    names
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn every_json_management_write_rejects_unknown_fields_after_valid_control(pool: PgPool) {
    use crate::providers::secrets::EnvSecrets;
    let mut f = fixture(&pool).await;
    f.s = f.s.with_secret_allowlists(
        EnvSecrets::from_lookup(["OPENAI_API_KEY".into()], |_| None),
        EnvSecrets::default(),
    );
    let ws = f.team;
    let model_id = model(&pool, "matrix-existing").await;
    let connection = Uuid::new_v4();
    let deployment = Uuid::new_v4();
    let catalog = Uuid::new_v4();
    let account = Uuid::new_v4();
    let mapping = Uuid::new_v4();
    let price_id = Uuid::new_v4();
    sqlx::query("INSERT INTO provider_connections(id,name,provider,credential_ref,enabled) VALUES($1,'Matrix','openai','env:OPENAI_API_KEY',true)").bind(connection).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO deployments(id,model_id,provider_connection_id,upstream_model,enabled) VALUES($1,$2,$3,'existing',true)").bind(deployment).bind(model_id).bind(connection).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO deployment_prices(id,deployment_id,input_microusd_per_million,output_microusd_per_million,input_token_limit,output_token_limit,pricing_version) VALUES($1,$2,1000000,1000000,100,100,1)").bind(price_id).bind(deployment).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO catalogs(id,name) VALUES($1,'Matrix')")
        .bind(catalog)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO catalog_models(catalog_id,model_id) VALUES($1,$2)")
        .bind(catalog)
        .bind(model_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO workspace_type_catalogs(kind,catalog_id) VALUES('team',$1)")
        .bind(catalog)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO service_accounts(id,workspace_id,name) VALUES($1,$2,'Existing')")
        .bind(account)
        .bind(ws)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO oidc_group_mappings(id,issuer,group_value,target_kind,platform_role,enabled) VALUES($1,'https://idp.example.test','existing','platform','user',false)").bind(mapping).execute(&pool).await.unwrap();
    let (status, k) = key(&f, &f.owner, ws, Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    let key_id = id(&k);
    let (status, reconciliation_key) = key(&f, &f.owner, ws, Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    let reconciliation_key_id = id(&reconciliation_key);
    let accepting = user(&pool, "accepting", "user").await;
    let (status, invitation) = call(
        &f.s,
        &f.owner,
        "POST",
        &format!("/api/v1/workspaces/{ws}/invitations"),
        json!({"email":accepting.email,"role":"member"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let execution = Uuid::new_v4();
    sqlx::query("INSERT INTO inference_executions(id,workspace_id,api_key_id,deployment_id,public_model,provider,streamed,state,root_request_id,attempt_number) VALUES($1,$2,$3,$4,'matrix-existing','openai',false,'succeeded',$1,1)").bind(execution).bind(ws).bind(reconciliation_key_id).bind(deployment).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO governance_reservations(execution_id,workspace_id,api_key_id,deployment_id,admitted_at,minute_start,month_start,lease_expires_at,state,held_microusd,unbounded_cost,price_id) SELECT $1,$2,$3,$4,e.started_at,date_trunc('minute',e.started_at,'UTC'),date_trunc('month',e.started_at,'UTC'),now()-interval '1 day','unknown',10,false,$5 FROM inference_executions e WHERE e.id=$1").bind(execution).bind(ws).bind(reconciliation_key_id).bind(deployment).bind(price_id).execute(&pool).await.unwrap();
    let cost_center = Uuid::new_v4();
    sqlx::query("INSERT INTO cost_centers(id,name,code) VALUES($1,'Existing','existing')")
        .bind(cost_center)
        .execute(&pool)
        .await
        .unwrap();
    let (status, rule) = call(
        &f.s,
        &f.admin,
        "POST",
        "/api/v1/platform/alerts/rules",
        json!({"name":"Existing","kind":"batch_failed","notify_platform_admins":true}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let rule_id = id(&rule);
    let (status, rule) = call(
        &f.s,
        &f.owner,
        "POST",
        &format!("/api/v1/workspaces/{ws}/alerts/rules"),
        json!({"name":"Existing","kind":"batch_failed","notify_workspace_admins":true}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let workspace_rule = id(&rule);
    let policy = json!({"requests_per_minute":null,"tokens_per_minute":null,"concurrent_requests":null,"budgets":[]});
    let price = json!({"input_microusd_per_million":"1000000","output_microusd_per_million":"1000000","input_token_limit":100,"output_token_limit":100});
    let platform = "/api/v1/platform";
    let workspace = format!("/api/v1/workspaces/{ws}");
    struct Case<'a> {
        handler: &'static str,
        method: &'static str,
        path: String,
        actor: &'a BrowserPrincipal,
        body: Value,
    }
    let mut cases = Vec::new();
    macro_rules! case {
        ($handler:literal,$method:literal,$path:expr,$actor:expr,$body:expr) => {
            cases.push(Case {
                handler: $handler,
                method: $method,
                path: $path.into(),
                actor: $actor,
                body: $body,
            });
        };
    }
    case!(
        "resources::create_provider",
        "POST",
        format!("{platform}/providers"),
        &f.admin,
        json!({"name":"New connection","provider":"openai","credential_ref":"env:OPENAI_API_KEY","enabled":false})
    );
    case!(
        "admin_tokens::create",
        "POST",
        format!("{platform}/admin-tokens"),
        &f.admin,
        json!({"name":"Matrix probe","scopes":["platform:read"],"expires_in_days":30})
    );
    case!(
        "resources::update_provider",
        "PATCH",
        format!("{platform}/providers/{connection}"),
        &f.admin,
        json!({"enabled":true})
    );
    case!(
        "resources::create_model",
        "POST",
        format!("{platform}/models"),
        &f.admin,
        json!({"public_name":"matrix-new","display_name":"New","enabled":false})
    );
    case!(
        "resources::update_model",
        "PATCH",
        format!("{platform}/models/{model_id}"),
        &f.admin,
        json!({"display_name":"Updated"})
    );
    case!(
        "resources::create_deployment",
        "POST",
        format!("{platform}/deployments"),
        &f.admin,
        json!({"model_id":model_id,"provider_connection_id":connection,"upstream_model":"matrix-new","enabled":false})
    );
    case!(
        "resources::update_deployment",
        "PATCH",
        format!("{platform}/deployments/{deployment}"),
        &f.admin,
        json!({"enabled":true})
    );
    case!(
        "setup::model_setup",
        "POST",
        format!("{platform}/model-setup"),
        &f.admin,
        json!({"model":{"public_name":"matrix-setup","display_name":"Setup","enabled":false},"route":{"provider_connection_id":connection,"upstream_model":"matrix-setup","enabled":false}})
    );
    case!(
        "catalogs::create_catalog",
        "POST",
        format!("{platform}/catalogs"),
        &f.admin,
        json!({"name":"New catalog"})
    );
    case!(
        "catalogs::update_catalog",
        "PATCH",
        format!("{platform}/catalogs/{catalog}"),
        &f.admin,
        json!({"name":"Updated catalog"})
    );
    case!(
        "catalogs::put_catalog_models",
        "PUT",
        format!("{platform}/catalogs/{catalog}/models"),
        &f.admin,
        json!({"model_ids":[model_id]})
    );
    case!(
        "catalogs::put_model_catalogs",
        "PUT",
        format!("{platform}/models/{model_id}/catalogs"),
        &f.admin,
        json!({"catalog_ids":[catalog]})
    );
    case!(
        "catalogs::put_type_catalogs",
        "PUT",
        format!("{platform}/workspace-types/team/catalogs"),
        &f.admin,
        json!({"catalog_ids":[catalog]})
    );
    case!(
        "catalogs::put_catalog_defaults",
        "PUT",
        format!("{platform}/catalog-defaults"),
        &f.admin,
        json!({"personal":[],"team":[catalog],"project":[]})
    );
    case!(
        "catalogs::put_workspace_catalogs",
        "PUT",
        format!("{platform}/workspaces/{ws}/catalogs"),
        &f.admin,
        json!({"mode":"replace","catalog_ids":[catalog]})
    );
    case!(
        "catalogs::select_model",
        "POST",
        format!("{workspace}/models"),
        &f.owner,
        json!({"model_id":model_id})
    );
    case!(
        "catalogs::direct_model",
        "POST",
        format!("{platform}/workspaces/{ws}/models"),
        &f.admin,
        json!({"model_id":model_id})
    );
    case!(
        "directory::create_workspace",
        "POST",
        format!("{platform}/workspaces"),
        &f.admin,
        json!({"name":"New team","kind":"team","owner_user_id":f.owner.user_id})
    );
    case!(
        "directory::update_workspace",
        "PATCH",
        format!("{platform}/workspaces/{ws}"),
        &f.admin,
        json!({"name":"Updated team"})
    );
    case!(
        "directory::update_own_workspace",
        "PATCH",
        workspace.clone(),
        &f.owner,
        json!({"name":"Owned team"})
    );
    case!(
        "directory::create_user",
        "POST",
        format!("{platform}/users"),
        &f.admin,
        json!({"email":"matrix-user@example.test","platform_role":"user"})
    );
    case!(
        "directory::update_user",
        "PATCH",
        format!("{platform}/users/{}", f.outsider.user_id),
        &f.admin,
        json!({"disabled":false})
    );
    case!(
        "directory::grant_role",
        "POST",
        format!("{platform}/users/{}/roles", f.outsider.user_id),
        &f.admin,
        json!({"role":"auditor"})
    );
    case!(
        "directory::create_mapping",
        "POST",
        format!("{platform}/oidc/group-mappings"),
        &f.admin,
        json!({"issuer":"https://idp.example.test","group_value":"new","target_kind":"platform","platform_role":"user","enabled":false})
    );
    case!(
        "directory::update_mapping",
        "PATCH",
        format!("{platform}/oidc/group-mappings/{mapping}"),
        &f.admin,
        json!({"enabled":false})
    );
    case!(
        "directory::create_cost_center",
        "POST",
        format!("{platform}/cost-centers"),
        &f.admin,
        json!({"name":"New center","code":"matrix-new"})
    );
    case!(
        "directory::update_cost_center",
        "PATCH",
        format!("{platform}/cost-centers/{cost_center}"),
        &f.admin,
        json!({"name":"Updated center","code":"matrix-updated"})
    );
    case!(
        "governance::policies::put_type_policy",
        "PUT",
        format!("{platform}/workspace-types/team/policy"),
        &f.admin,
        policy.clone()
    );
    case!(
        "governance::policies::put_platform_workspace_policy",
        "PUT",
        format!("{platform}/workspaces/{ws}/policy"),
        &f.admin,
        policy.clone()
    );
    case!(
        "governance::policies::put_workspace_policy",
        "PUT",
        format!("{workspace}/policy"),
        &f.owner,
        policy.clone()
    );
    case!(
        "governance::policies::put_key_policy",
        "PUT",
        format!("{workspace}/keys/{key_id}/policy"),
        &f.owner,
        policy.clone()
    );
    case!(
        "governance::prices::create_price",
        "POST",
        format!("{platform}/deployments/{deployment}/prices"),
        &f.admin,
        price
    );
    case!(
        "governance::prices::put_model_routing",
        "PUT",
        format!("{platform}/models/{model_id}/routing"),
        &f.admin,
        json!({"strategy":"weighted","max_attempts":2,"allow_ambiguous_failover":false,"required_residency":null})
    );
    case!(
        "governance::prices::put_deployment_routing",
        "PUT",
        format!("{platform}/deployments/{deployment}/routing"),
        &f.admin,
        json!({"priority":0,"weight":1,"residency":null,"failure_threshold":3,"cooldown_seconds":30})
    );
    case!(
        "batch_scheduling::write",
        "PUT",
        format!("{platform}/deployments/{deployment}/batch-scheduling"),
        &f.admin,
        json!({"max_concurrency":2})
    );
    case!(
        "keys::create_key",
        "POST",
        format!("{workspace}/keys"),
        &f.owner,
        json!({"name":"New key","expires_in_days":1})
    );
    case!(
        "keys::update_key",
        "PATCH",
        format!("{workspace}/keys/{key_id}"),
        &f.owner,
        json!({"disabled":false})
    );
    case!(
        "keys::rotate_key",
        "POST",
        format!("{workspace}/keys/{key_id}/rotate"),
        &f.owner,
        json!({"expires_in_days":1})
    );
    case!(
        "keys::create_account",
        "POST",
        format!("{workspace}/service-accounts"),
        &f.owner,
        json!({"name":"New account"})
    );
    case!(
        "keys::update_account",
        "PATCH",
        format!("{workspace}/service-accounts/{account}"),
        &f.owner,
        json!({"disabled":false})
    );
    case!(
        "members::platform_add_member",
        "POST",
        format!("{platform}/workspaces/{ws}/members"),
        &f.admin,
        json!({"user_id":f.auditor.user_id,"role":"member"})
    );
    case!(
        "members::add_workspace_member",
        "POST",
        format!("{workspace}/members"),
        &f.owner,
        json!({"user_id":f.outsider.user_id,"role":"member"})
    );
    case!(
        "members::invite",
        "POST",
        format!("{workspace}/invitations"),
        &f.owner,
        json!({"email":"matrix-invite@example.test","role":"member"})
    );
    case!(
        "members::accept_invite",
        "POST",
        "/api/v1/invitations/accept",
        &accepting,
        json!({"token":invitation["token"]})
    );
    case!(
        "legal_holds::place",
        "POST",
        format!("{platform}/legal-holds"),
        &f.admin,
        json!({"user_id":f.outsider.user_id,"reference":"MATRIX-1"})
    );
    case!(
        "alerts::create_platform_rule",
        "POST",
        format!("{platform}/alerts/rules"),
        &f.admin,
        json!({"name":"New platform rule","kind":"batch_failed","notify_platform_admins":true})
    );
    case!(
        "alerts::update_platform_rule",
        "PUT",
        format!("{platform}/alerts/rules/{rule_id}"),
        &f.admin,
        json!({"name":"Updated platform rule","kind":"batch_failed","notify_platform_admins":true})
    );
    case!(
        "alerts::create_workspace_rule",
        "POST",
        format!("{workspace}/alerts/rules"),
        &f.owner,
        json!({"name":"New workspace rule","kind":"batch_failed","notify_workspace_admins":true})
    );
    case!(
        "alerts::update_workspace_rule",
        "PUT",
        format!("{workspace}/alerts/rules/{workspace_rule}"),
        &f.owner,
        json!({"name":"Updated workspace rule","kind":"batch_failed","notify_workspace_admins":true})
    );
    case!(
        "alerts::mark_read",
        "POST",
        "/api/v1/me/notifications/read",
        &f.owner,
        json!({"all":true})
    );
    case!(
        "settings::update_general",
        "PUT",
        format!("{platform}/settings/general"),
        &f.admin,
        json!({"display_name":"Matrix","human_key_max_lifetime_days":30})
    );
    case!(
        "settings::update_privacy",
        "PUT",
        format!("{platform}/settings/privacy"),
        &f.admin,
        json!({"openrouter_data_collection":"deny"})
    );
    case!(
        "settings::update_email",
        "PUT",
        format!("{platform}/settings/email"),
        &f.admin,
        json!({"host":null})
    );
    case!(
        "settings::storage::update_storage",
        "PUT",
        format!("{platform}/settings/storage"),
        &f.admin,
        json!({"batch":{"enabled":false,"retention_days":7},"video":{"enabled":false,"retention_days":7},"user_files":{"enabled":false,"retention_days":30},"export":{"retention_days":1}})
    );
    // Reconciliation uses a separate key so rotation above cannot retire its owner.
    case!(
        "governance::reports::reconcile",
        "POST",
        format!("{workspace}/costs/{execution}/reconcile"),
        &f.admin,
        json!({"input_tokens":"1","output_tokens":"1","billing_usage":null,"evidence":"configuration control"})
    );
    let cost_upload = super::provider_cost_tests::upload();
    let (status, cost_import) = call(&f.s, &f.admin, "POST", "/api/v1/platform/provider-cost-imports", cost_upload.clone()).await;
    assert_eq!(status, StatusCode::OK, "{cost_import}");
    let cost_mapping = super::provider_cost_tests::mapping(connection, Some(ws));
    let (status, cost_map) = call(&f.s, &f.admin, "POST", "/api/v1/platform/provider-cost-mappings", cost_mapping.clone()).await;
    assert_eq!(status, StatusCode::OK, "{cost_map}");
    let mut next_mapping = cost_mapping;
    next_mapping["supersedes"] = cost_map["id"].clone();
    case!("provider_costs::import", "POST", "/api/v1/platform/provider-cost-imports", &f.admin, cost_upload);
    case!("provider_costs::create_mapping", "POST", "/api/v1/platform/provider-cost-mappings", &f.admin, next_mapping);
    case!("provider_costs::reconcile", "POST", "/api/v1/platform/provider-reconciliations", &f.admin,
        json!({"import_id":id(&cost_import),"mapping_id":id(&cost_map)}));
    let covered: BTreeSet<String> = cases.iter().map(|c| c.handler.to_owned()).collect();
    assert_eq!(
        covered,
        json_handlers(),
        "every JSON write extractor needs an exercised case"
    );
    for case in cases {
        let (status, _) = call(&f.s, case.actor, case.method, &case.path, case.body.clone()).await;
        assert!(
            status.is_success(),
            "{} {}: control returned {status}",
            case.method,
            case.handler
        );
        for (name, value) in [
            ("environment_variables", json!({"UI_PASSWORD":"attacker"})),
            ("UI_LOGO_PATH", json!("/etc/passwd")),
            ("plugin", json!("s3://attacker/module.py")),
        ] {
            let mut extra = case.body.clone();
            extra[name] = value;
            assert_eq!(
                call(&f.s, case.actor, case.method, &case.path, extra)
                    .await
                    .0,
                StatusCode::UNPROCESSABLE_ENTITY,
                "{} {}",
                case.method,
                case.handler
            );
        }
    }
}
