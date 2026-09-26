use serde::Serialize;

use crate::{auth::Principal, store::Store};

#[derive(Serialize, sqlx::FromRow)]
pub struct Model {
    pub id: String,
    pub created: i64,
    pub owned_by: String,
    #[sqlx(skip)]
    pub object: &'static str,
}

impl Store {
    pub async fn visible_models(&self, principal: &Principal) -> Result<Vec<Model>, sqlx::Error> {
        let mut models = sqlx::query_as::<_, Model>(
            r#"
            SELECT g.public_name AS id, extract(epoch FROM m.created_at)::bigint AS created,
                   'platform'::text AS owned_by
            FROM models m
            JOIN organization_model_grants g ON g.model_id=m.id AND g.organization_id=$1
            JOIN workspaces w ON w.organization_id=g.organization_id AND w.id=$2 AND w.disabled_at IS NULL
            JOIN api_keys k ON k.organization_id=$1 AND k.workspace_id=$2 AND k.id=$4
                AND k.revoked_at IS NULL AND (k.expires_at IS NULL OR k.expires_at>now())
            WHERE m.enabled AND (
                NOT EXISTS (SELECT 1 FROM key_model_restrictions r WHERE r.organization_id=$1
                    AND r.workspace_id=$2 AND r.governance_key_id=k.governance_key_id)
                OR EXISTS (SELECT 1 FROM key_model_selections s WHERE s.organization_id=$1
                    AND s.workspace_id=$2 AND s.governance_key_id=k.governance_key_id AND s.model_id=m.id)
              ) AND (
                EXISTS (SELECT 1 FROM workspace_model_grants wg WHERE wg.organization_id=$1 AND wg.workspace_id=$2 AND wg.model_id=m.id)
                OR (w.kind='personal' AND w.owner_user_id=$3 AND EXISTS (
                    SELECT 1 FROM user_model_grants ug JOIN organization_memberships om
                    ON om.organization_id=ug.organization_id AND om.user_id=ug.user_id AND om.disabled_at IS NULL
                    JOIN users u ON u.id=ug.user_id AND u.disabled_at IS NULL
                    WHERE ug.organization_id=$1 AND ug.user_id=$3 AND ug.model_id=m.id
                ))
              )
              AND EXISTS (
                SELECT 1 FROM deployments d
                JOIN provider_connections p
                  ON p.id = d.provider_connection_id
                WHERE d.model_id = m.id
                  AND d.enabled AND p.enabled
              )
            ORDER BY g.public_name
            "#,
        )
        .bind(principal.organization_id)
        .bind(principal.workspace_id)
        .bind(principal.user_id)
        .bind(principal.key_id)
        .fetch_all(&self.pool)
        .await?;
        for model in &mut models {
            model.object = "model";
        }
        Ok(models)
    }
}
