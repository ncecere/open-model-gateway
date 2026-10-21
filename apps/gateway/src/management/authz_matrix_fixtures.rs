//! Existing rows, not random missing-object UUIDs, for the authorization probes.
use super::*;
use futures::StreamExt;
use std::collections::BTreeMap;

pub(super) struct Targets(BTreeMap<&'static str, String>);
async fn create(f: &Fixture, actor: &BrowserPrincipal, path: &str, body: Value) -> String {
    let (status, value) = call(&f.s, actor, "POST", path, body).await;
    assert!(status.is_success(), "fixture creation: {path}: {status}");
    value["id"].as_str().unwrap().to_owned()
}
impl Targets {
    pub(super) async fn new(pool: &PgPool, f: &Fixture, key: Uuid, owner: &Session) -> Self {
        let model = model(pool, "matrix-model").await;
        direct(pool, f.team, model).await;
        let group = Uuid::new_v4();
        sqlx::query("INSERT INTO scim_groups(id,display_name) VALUES($1,'Matrix')")
            .bind(group)
            .execute(pool)
            .await
            .unwrap();
        let provider = Uuid::new_v4();
        let deployment = Uuid::new_v4();
        sqlx::query("INSERT INTO provider_connections(id,name,provider,credential_ref) VALUES($1,'Matrix disabled','openai','env:NOT_RESOLVED')")
            .bind(provider).execute(pool).await.unwrap();
        sqlx::query("INSERT INTO deployments(id,model_id,provider_connection_id,upstream_model) VALUES($1,$2,$3,'matrix-disabled')")
            .bind(deployment).bind(model).bind(provider).execute(pool).await.unwrap();
        let execution = Uuid::new_v4();
        let video_execution = Uuid::new_v4();
        for execution in [execution, video_execution] {
            sqlx::query("INSERT INTO inference_executions(id,workspace_id,api_key_id,deployment_id,public_model,provider,streamed,state,root_request_id,input_tokens,output_tokens,client_session_id) VALUES($1,$2,$3,$4,'matrix-model','openai',false,'succeeded',$1,5,1,'matrix-session')")
            .bind(execution).bind(f.team).bind(key).bind(deployment).execute(pool).await.unwrap();
        }
        let batch = Uuid::new_v4();
        let video = Uuid::new_v4();
        sqlx::query("INSERT INTO async_jobs(id,kind,workspace_id,api_key_id,deployment_id,execution_id,public_model,provider,upstream_id,state,upstream_status,poll_deadline_at,batch_endpoint,user_id,request_total,request_completed,request_failed,completed_at,settled_at) VALUES($1,'batch',$2,$3,$4,$5,'matrix-model','openai','matrix-batch','completed','completed',now()+interval '1 hour','/v1/chat/completions',$6,1,1,0,now(),now())")
            .bind(batch).bind(f.team).bind(key).bind(deployment).bind(execution).bind(f.owner.user_id).execute(pool).await.unwrap();
        sqlx::query("INSERT INTO async_jobs(id,kind,workspace_id,api_key_id,deployment_id,execution_id,public_model,provider,upstream_id,state,upstream_status,poll_deadline_at,video_seconds,video_size,completed_at,settled_at) VALUES($1,'video',$2,$3,$4,$5,'matrix-model','openai','matrix-video','completed','completed',now()+interval '1 hour',8,'720x1280',now(),now())")
            .bind(video).bind(f.team).bind(key).bind(deployment).bind(video_execution).execute(pool).await.unwrap();
        sqlx::query(
            "UPDATE installation_settings SET file_batch_enabled=true,file_user_files_enabled=true",
        )
        .execute(pool)
        .await
        .unwrap();
        let file_runtime = crate::filestore::FileStoreRuntime::memory();
        let storage = crate::filestore::files::FileStorage::new(f.s.clone(), file_runtime);
        let file = storage
            .create(
                crate::filestore::files::NewFile {
                    created_by_user_id: Some(f.owner.user_id),
                    api_purpose: Some("user_data".to_owned()),
                    ..crate::filestore::files::NewFile::new(
                        crate::filestore::Purpose::UserFile,
                        Some(f.team),
                    )
                },
                futures::stream::iter([Ok(bytes::Bytes::from_static(b"matrix fixture"))]).boxed(),
            )
            .await
            .unwrap();
        let session_id: Uuid =
            sqlx::query_scalar("SELECT id FROM browser_sessions WHERE token_hash=$1")
                .bind(Sha256::digest(owner.cookie.as_bytes()).as_slice())
                .fetch_one(pool)
                .await
                .unwrap();
        let mut rows = BTreeMap::from([
            ("key", key.to_string()),
            ("group", group.to_string()),
            ("model", model.to_string()),
            ("provider", provider.to_string()),
            ("deployment", deployment.to_string()),
            ("execution", execution.to_string()),
            ("root", execution.to_string()),
            ("session", session_id.to_string()),
            ("file", crate::filestore::files::public_id(file.id)),
            ("batch", crate::jobs::types::client_id("batch_", batch)),
            ("video", crate::jobs::types::client_id("video_", video)),
        ]);
        for (name, actor, path, body) in [
            (
                "catalog",
                &f.admin,
                "/api/v1/platform/catalogs".to_owned(),
                json!({"name":"Matrix"}),
            ),
            (
                "cost-center",
                &f.admin,
                "/api/v1/platform/cost-centers".to_owned(),
                json!({"name":"Matrix","code":"MATRIX"}),
            ),
            (
                "account",
                &f.owner,
                format!("/api/v1/workspaces/{}/service-accounts", f.team),
                json!({"name":"Matrix"}),
            ),
            (
                "invitation",
                &f.owner,
                format!("/api/v1/workspaces/{}/invitations", f.team),
                json!({"email":"invited@example.test","role":"member"}),
            ),
            (
                "platform-rule",
                &f.admin,
                "/api/v1/platform/alerts/rules".to_owned(),
                json!({"name":"Matrix","kind":"budget_threshold","budget_layers":["type","local"],"thresholds":[80]}),
            ),
            (
                "workspace-rule",
                &f.owner,
                format!("/api/v1/workspaces/{}/alerts/rules", f.team),
                json!({"name":"Matrix","kind":"budget_threshold","budget_layers":["local"],"thresholds":[80]}),
            ),
            (
                "mapping",
                &f.admin,
                "/api/v1/platform/oidc/group-mappings".to_owned(),
                json!({"issuer":"https://matrix.invalid","group_value":"unused","target_kind":"platform","platform_role":"user","enabled":false}),
            ),
            (
                "hold",
                &f.admin,
                "/api/v1/platform/legal-holds".to_owned(),
                json!({"user_id":f.member.user_id,"reference":"MATRIX-195"}),
            ),
        ] {
            rows.insert(name, create(f, actor, &path, body).await);
        }
        Self(rows)
    }
    pub(super) fn concrete(&self, path: &str, f: &Fixture) -> String {
        let id = if path.contains("/files") {
            &self.0["file"]
        } else if path.contains("/batches") {
            &self.0["batch"]
        } else if path.contains("/videos") {
            &self.0["video"]
        } else if path.contains("/alerts") {
            &self.0[if path.contains("/platform/") {
                "platform-rule"
            } else {
                "workspace-rule"
            }]
        } else if path == "/scim/v2/ResourceTypes/{id}" {
            "User"
        } else if path == "/scim/v2/Schemas/{id}" {
            "urn:ietf:params:scim:schemas:core:2.0:User"
        } else if path == "/scim/v2/Groups/{id}" {
            &self.0["group"]
        } else if path == "/api/v1/platform/models/{id}/catalogs" {
            &self.0["model"]
        } else if path.contains("/catalogs") {
            &self.0["catalog"]
        } else if path.contains("/providers") {
            &self.0["provider"]
        } else if path.contains("/deployments") {
            &self.0["deployment"]
        } else if path.contains("/models") {
            &self.0["model"]
        } else if path.contains("/invitations") {
            &self.0["invitation"]
        } else if path.contains("/service-accounts") {
            &self.0["account"]
        } else if path.contains("/cost-centers") {
            &self.0["cost-center"]
        } else if path.contains("/group-mappings") {
            &self.0["mapping"]
        } else {
            &f.member.user_id.to_string()
        };
        let mut result = path
            .replace("{id}", id)
            .replace("{ws}", &f.team.to_string())
            .replace("{user}", &f.member.user_id.to_string())
            .replace("{kind}", "team")
            .replace("{role}", "user")
            .replace("{*rest}", "unknown");
        for name in [
            "key",
            "model",
            "deployment",
            "execution",
            "root",
            "hold",
            "session",
        ] {
            let value = if name == "session" && !path.starts_with("/api/v1/me/sessions") {
                "matrix-session"
            } else {
                &self.0[name]
            };
            result = result.replace(&format!("{{{name}}}"), value);
        }
        result
    }
}
