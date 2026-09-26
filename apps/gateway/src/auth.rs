use rand::{RngCore, rngs::OsRng};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use uuid::Uuid;

use crate::store::Store;

#[derive(Clone, Copy, Debug)]
pub struct Principal {
    pub key_id: Uuid,
    pub organization_id: Uuid,
    pub workspace_id: Uuid,
    pub user_id: Option<Uuid>,
}

/// Secret-bearing values intentionally do not implement Debug or Serialize.
pub struct NewApiKey {
    pub id: Uuid,
    pub token: String,
    pub digest: [u8; 32],
}

impl NewApiKey {
    pub fn generate() -> Self {
        let id = Uuid::new_v4();
        let mut secret = [0u8; 32];
        OsRng.fill_bytes(&mut secret);
        let token = format!("omg_{}.{}", id.simple(), hex::encode(secret));
        let digest = digest(&token);
        Self { id, token, digest }
    }
}

fn digest(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}

fn token_id(token: &str) -> Option<Uuid> {
    if token.len() != 101 {
        return None;
    }
    let (id, secret) = token.strip_prefix("omg_")?.split_once('.')?;
    if id.len() != 32
        || secret.len() != 64
        || !id
            .bytes()
            .chain(secret.bytes())
            .all(|b| b.is_ascii_hexdigit())
    {
        return None;
    }
    Uuid::parse_str(id).ok()
}

#[derive(sqlx::FromRow)]
struct KeyRecord {
    id: Uuid,
    organization_id: Uuid,
    workspace_id: Uuid,
    issued_to_user_id: Option<Uuid>,
    secret_hash: Vec<u8>,
}

impl Store {
    /// No authorization cache: membership removal and revocation apply on the next request.
    pub async fn authenticate(&self, token: &str) -> Result<Option<Principal>, sqlx::Error> {
        let Some(id) = token_id(token) else {
            return Ok(None);
        };
        let record = sqlx::query_as::<_, KeyRecord>(
            r#"
            SELECT k.id, k.organization_id, k.workspace_id, k.issued_to_user_id, k.secret_hash
            FROM api_keys k
            JOIN organizations o ON o.id = k.organization_id AND o.disabled_at IS NULL
            LEFT JOIN users u ON u.id = k.issued_to_user_id AND u.disabled_at IS NULL
            LEFT JOIN organization_memberships om
              ON om.organization_id = k.organization_id AND om.user_id = k.issued_to_user_id
             AND om.disabled_at IS NULL
            JOIN workspaces w ON w.organization_id = k.organization_id AND w.id = k.workspace_id
             AND w.disabled_at IS NULL
            WHERE k.id = $1 AND k.revoked_at IS NULL
              AND (k.expires_at IS NULL OR k.expires_at > now())
              AND (
                (k.issued_to_user_id IS NOT NULL AND u.id IS NOT NULL AND om.user_id IS NOT NULL AND (
                (w.kind = 'personal' AND w.owner_user_id = k.issued_to_user_id)
                OR (w.kind IN ('team','project') AND EXISTS (
                    SELECT 1 FROM workspace_memberships wm
                    WHERE wm.organization_id = k.organization_id AND wm.workspace_id = k.workspace_id
                      AND wm.user_id = k.issued_to_user_id AND wm.disabled_at IS NULL
                ))))
                OR (k.service_account_id IS NOT NULL AND w.kind IN ('team','project') AND EXISTS (
                    SELECT 1 FROM service_accounts sa WHERE sa.organization_id=k.organization_id
                    AND sa.workspace_id=k.workspace_id AND sa.id=k.service_account_id AND sa.disabled_at IS NULL
                ))
              )
            "#,
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        let Some(record) = record else {
            return Ok(None);
        };
        if !bool::from(record.secret_hash.as_slice().ct_eq(&digest(token))) {
            return Ok(None);
        }
        Ok(Some(Principal {
            key_id: record.id,
            organization_id: record.organization_id,
            workspace_id: record.workspace_id,
            user_id: record.issued_to_user_id,
        }))
    }
}

/// Admission runs after the shared catalog lock and consuming organization lock.
/// SHARE (not KEY SHARE) prevents non-key revocations while the reservation commits.
/// Separate inner-join queries avoid locking the nullable side of an outer join.
pub(crate) async fn revalidate(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    principal: &Principal,
) -> Result<Option<Uuid>, sqlx::Error> {
    let row: Option<(Uuid, Option<Uuid>, String, Option<Uuid>)> = sqlx::query_as(
        "SELECT k.governance_key_id,k.service_account_id,w.kind,w.owner_user_id
         FROM api_keys k JOIN workspaces w ON w.organization_id=k.organization_id AND w.id=k.workspace_id
         JOIN organizations o ON o.id=k.organization_id
         WHERE k.organization_id=$1 AND k.workspace_id=$2 AND k.id=$3
         AND k.issued_to_user_id IS NOT DISTINCT FROM $4::uuid
         AND k.revoked_at IS NULL AND (k.expires_at IS NULL OR k.expires_at>clock_timestamp())
         AND w.disabled_at IS NULL AND o.disabled_at IS NULL FOR SHARE OF k,w",
    ).bind(principal.organization_id).bind(principal.workspace_id).bind(principal.key_id)
        .bind(principal.user_id).fetch_optional(&mut **tx).await?;
    let Some((lineage, service_account, kind, owner)) = row else {
        return Ok(None);
    };
    if let Some(user) = principal.user_id {
        let active: Option<Uuid> = sqlx::query_scalar(
            "SELECT u.id FROM users u JOIN organization_memberships om ON om.user_id=u.id
             WHERE u.id=$1 AND om.organization_id=$2 AND u.disabled_at IS NULL AND om.disabled_at IS NULL
             FOR SHARE OF u,om",
        ).bind(user).bind(principal.organization_id).fetch_optional(&mut **tx).await?;
        if active.is_none() {
            return Ok(None);
        }
        if kind == "personal" {
            return Ok((owner == Some(user)).then_some(lineage));
        }
        if !matches!(kind.as_str(), "team" | "project") {
            return Ok(None);
        }
        let member: Option<Uuid> = sqlx::query_scalar(
            "SELECT user_id FROM workspace_memberships WHERE organization_id=$1 AND workspace_id=$2
             AND user_id=$3 AND disabled_at IS NULL FOR SHARE",
        )
        .bind(principal.organization_id)
        .bind(principal.workspace_id)
        .bind(user)
        .fetch_optional(&mut **tx)
        .await?;
        Ok(member.map(|_| lineage))
    } else if matches!(kind.as_str(), "team" | "project") {
        let account: Option<Uuid> = sqlx::query_scalar(
            "SELECT id FROM service_accounts WHERE organization_id=$1 AND workspace_id=$2
             AND id=$3 AND disabled_at IS NULL FOR SHARE",
        )
        .bind(principal.organization_id)
        .bind(principal.workspace_id)
        .bind(service_account)
        .fetch_optional(&mut **tx)
        .await?;
        Ok(account.map(|_| lineage))
    } else {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_have_expected_format_and_independent_random_secrets() {
        let first = NewApiKey::generate();
        let second = NewApiKey::generate();
        assert_eq!(token_id(&first.token), Some(first.id));
        assert_eq!(first.digest, digest(&first.token));
        assert_ne!(first.digest, second.digest);
        assert_ne!(first.id, second.id);
    }

    #[test]
    fn malformed_tokens_are_rejected_before_database_access() {
        for token in ["", "omg_bad.secret", "Bearer abc", &"é".repeat(101)] {
            assert!(token_id(token).is_none());
        }
        let valid = NewApiKey::generate();
        assert!(token_id(&valid.token.replace('.', ":")).is_none());
        assert!(token_id(&valid.token.replacen("omg_", "sk__", 1)).is_none());
    }
}
