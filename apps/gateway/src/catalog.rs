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
    /// Models the key may call now. A lock-free read-only snapshot on the
    /// primary (`crate::reporting`; never the reporting replica): live key
    /// revalidation and catalog eligibility in one snapshot, without the
    /// catalog or installation lock. A listing is advisory; admission
    /// re-checks everything under its locks. Empty when the key is no longer
    /// valid; see [`Store::key_models`].
    pub async fn visible_models(&self, p: &Principal) -> Result<Vec<Model>, sqlx::Error> {
        Ok(self.key_models(p).await?.unwrap_or_default())
    }

    /// [`Store::visible_models`] for `/v1/models`: `None` when the key is no
    /// longer valid (for example a stale per-replica key-cache hit), which the
    /// caller refuses exactly as authentication would; this replica's cached
    /// entries of the key are dropped.
    pub async fn key_models(&self, p: &Principal) -> Result<Option<Vec<Model>>, sqlx::Error> {
        let mut tx = self.snapshot().await?;
        if crate::auth::revalidate_snapshot(&mut tx, p)
            .await?
            .is_none()
        {
            self.caches.forget_key(p.key_id);
            return Ok(None);
        }
        let mut models=sqlx::query_as::<_,Model>("SELECT m.public_name AS id,extract(epoch FROM m.created_at)::bigint AS created,'platform'::text AS owned_by FROM models m JOIN api_keys k ON k.id=$2 AND k.workspace_id=$1 WHERE workspace_model_allowed($1,m.id) AND (NOT EXISTS(SELECT 1 FROM key_model_restrictions r WHERE r.workspace_id=$1 AND r.governance_key_id=k.governance_key_id) OR EXISTS(SELECT 1 FROM key_model_selections s WHERE s.workspace_id=$1 AND s.governance_key_id=k.governance_key_id AND s.model_id=m.id)) AND EXISTS(SELECT 1 FROM deployments d JOIN provider_connections pc ON pc.id=d.provider_connection_id WHERE d.model_id=m.id AND d.enabled AND pc.enabled) ORDER BY m.public_name").bind(p.workspace_id).bind(p.key_id).fetch_all(&mut *tx).await?;
        tx.commit().await?;
        for m in &mut models {
            m.object = "model"
        }
        Ok(Some(models))
    }
}
