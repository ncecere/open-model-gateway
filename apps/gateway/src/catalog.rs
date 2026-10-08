use crate::{auth::Principal, store::Store};
use serde::Serialize;

#[derive(Serialize, sqlx::FromRow)]
pub struct Model {
    pub id: String,
    pub created: i64,
    pub owned_by: String,
    #[sqlx(skip)]
    pub object: &'static str,
}
impl Store {
    pub async fn visible_models(&self, p: &Principal) -> Result<Vec<Model>, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        // Same ordering as admission: catalog before installation, then live credential authority.
        sqlx::query("SELECT pg_advisory_xact_lock_shared(72419502)")
            .execute(&mut *tx)
            .await?;
        sqlx::query("SELECT lock_installation()")
            .execute(&mut *tx)
            .await?;
        if crate::auth::revalidate(&mut tx, p).await?.is_none() {
            return Ok(Vec::new());
        }
        let mut models=sqlx::query_as::<_,Model>("SELECT m.public_name AS id,extract(epoch FROM m.created_at)::bigint AS created,'platform'::text AS owned_by FROM models m JOIN api_keys k ON k.id=$2 AND k.workspace_id=$1 WHERE workspace_model_allowed($1,m.id) AND (NOT EXISTS(SELECT 1 FROM key_model_restrictions r WHERE r.workspace_id=$1 AND r.governance_key_id=k.governance_key_id) OR EXISTS(SELECT 1 FROM key_model_selections s WHERE s.workspace_id=$1 AND s.governance_key_id=k.governance_key_id AND s.model_id=m.id)) AND EXISTS(SELECT 1 FROM deployments d JOIN provider_connections pc ON pc.id=d.provider_connection_id WHERE d.model_id=m.id AND d.enabled AND pc.enabled) ORDER BY m.public_name").bind(p.workspace_id).bind(p.key_id).fetch_all(&mut *tx).await?;
        tx.commit().await?;
        for m in &mut models {
            m.object = "model"
        }
        Ok(models)
    }
}
