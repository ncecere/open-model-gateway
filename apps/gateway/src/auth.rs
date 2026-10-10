use rand::{RngCore, rngs::OsRng};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use uuid::Uuid;

use crate::store::Store;

#[derive(Clone, Copy, Debug)]
pub struct Principal {
    pub key_id: Uuid,
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
    workspace_id: Uuid,
    issued_to_user_id: Option<Uuid>,
    secret_hash: Vec<u8>,
}

impl Store {
    /// No authorization cache: entitlement, membership and revocation are live.
    pub async fn authenticate(&self, token: &str) -> Result<Option<Principal>, sqlx::Error> {
        let Some(id) = token_id(token) else {
            return Ok(None);
        };
        let record = sqlx::query_as::<_, KeyRecord>(
            "SELECT k.id,k.workspace_id,k.issued_to_user_id,k.secret_hash FROM api_keys k
             JOIN workspaces w ON w.id=k.workspace_id AND w.disabled_at IS NULL
             WHERE k.id=$1 AND k.revoked_at IS NULL AND k.disabled_at IS NULL AND (k.expires_at IS NULL OR k.expires_at>now())
             AND ((k.issued_to_user_id IS NOT NULL AND EXISTS(SELECT 1 FROM effective_platform_roles p WHERE p.user_id=k.issued_to_user_id)
               AND ((w.kind='personal' AND w.owner_user_id=k.issued_to_user_id) OR (w.kind IN ('team','project') AND EXISTS(
                 SELECT 1 FROM effective_workspace_memberships m WHERE m.workspace_id=w.id AND m.user_id=k.issued_to_user_id))))
               OR (k.service_account_id IS NOT NULL AND w.kind IN ('team','project') AND EXISTS(
                 SELECT 1 FROM service_accounts a WHERE a.workspace_id=w.id AND a.id=k.service_account_id AND a.disabled_at IS NULL)))",
        ).bind(id).fetch_optional(&self.pool).await?;
        let Some(record) = record else {
            return Ok(None);
        };
        if !bool::from(record.secret_hash.as_slice().ct_eq(&digest(token))) {
            return Ok(None);
        }
        Ok(Some(Principal {
            key_id: record.id,
            workspace_id: record.workspace_id,
            user_id: record.issued_to_user_id,
        }))
    }
}

/// Caller holds catalog advisory lock (72419502), then installation row lock.
/// SHARE prevents concurrent non-key revocation until admission commits.
pub(crate) async fn revalidate(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    principal: &Principal,
) -> Result<Option<Uuid>, sqlx::Error> {
    revalidate_with(tx, principal, true).await
}

/// [`revalidate`] in one round trip, for admission under the installation
/// lock: the same live rules and the same `FOR SHARE` row locks (each locking
/// CTE is fully read through `count(*)`, so every matching row is locked as
/// the multi-statement form does). Equality with [`revalidate`] is tested.
pub(crate) async fn revalidate_admission(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    principal: &Principal,
) -> Result<Option<Uuid>, sqlx::Error> {
    type Row = (Uuid, String, Option<Uuid>, i64, i64, i64, i64);
    let row: Option<Row> = sqlx::query_as(
        "WITH k AS MATERIALIZED (SELECT k.governance_key_id lineage,k.service_account_id account,w.kind,w.owner_user_id owner FROM api_keys k
          JOIN workspaces w ON w.id=k.workspace_id WHERE k.workspace_id=$1 AND k.id=$2
          AND k.issued_to_user_id IS NOT DISTINCT FROM $3::uuid
          AND k.revoked_at IS NULL AND k.disabled_at IS NULL AND (k.expires_at IS NULL OR k.expires_at>clock_timestamp())
          AND w.disabled_at IS NULL FOR SHARE OF k,w),
        u AS MATERIALIZED (SELECT id FROM users WHERE $3::uuid IS NOT NULL AND EXISTS(SELECT 1 FROM k) AND id=$3 AND disabled_at IS NULL AND cleaned_at IS NULL FOR SHARE),
        r AS MATERIALIZED (SELECT id FROM platform_role_grants WHERE EXISTS(SELECT 1 FROM u) AND user_id=$3 AND revoked_at IS NULL FOR SHARE),
        g AS MATERIALIZED (SELECT id FROM workspace_membership_grants WHERE EXISTS(SELECT 1 FROM r) AND EXISTS(SELECT 1 FROM k WHERE kind IN('team','project'))
          AND workspace_id=$1 AND user_id=$3 AND revoked_at IS NULL FOR SHARE),
        s AS MATERIALIZED (SELECT id FROM service_accounts WHERE $3::uuid IS NULL AND EXISTS(SELECT 1 FROM k WHERE kind IN('team','project'))
          AND workspace_id=$1 AND id=(SELECT account FROM k) AND disabled_at IS NULL FOR SHARE)
        SELECT k.lineage,k.kind,k.owner,(SELECT count(*) FROM u),(SELECT count(*) FROM r),(SELECT count(*) FROM g),(SELECT count(*) FROM s) FROM k",
    )
    .bind(principal.workspace_id)
    .bind(principal.key_id)
    .bind(principal.user_id)
    .fetch_optional(&mut **tx)
    .await?;
    let Some((lineage, kind, owner, user, roles, grants, account)) = row else {
        return Ok(None);
    };
    let shared = matches!(kind.as_str(), "team" | "project");
    Ok(match principal.user_id {
        Some(_) if user == 0 || roles == 0 => None,
        Some(u) if kind == "personal" => (owner == Some(u)).then_some(lineage),
        Some(_) if shared => (grants > 0).then_some(lineage),
        Some(_) => None,
        None if shared => (account > 0).then_some(lineage),
        None => None,
    })
}

/// [`revalidate`] without row locks, for read-only snapshot reads
/// (`crate::reporting`, e.g. `/v1/models`): the same live rules evaluated in
/// the caller's snapshot. Never for admission, which must lock.
pub(crate) async fn revalidate_snapshot(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    principal: &Principal,
) -> Result<Option<Uuid>, sqlx::Error> {
    revalidate_with(tx, principal, false).await
}

async fn revalidate_with(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    principal: &Principal,
    lock: bool,
) -> Result<Option<Uuid>, sqlx::Error> {
    let share = |sql: &'static str, locked: &'static str| if lock { locked } else { sql };
    let row: Option<(Uuid, Option<Uuid>, String, Option<Uuid>)> = sqlx::query_as(share(
        "SELECT k.governance_key_id,k.service_account_id,w.kind,w.owner_user_id FROM api_keys k
         JOIN workspaces w ON w.id=k.workspace_id WHERE k.workspace_id=$1 AND k.id=$2
         AND k.issued_to_user_id IS NOT DISTINCT FROM $3::uuid
         AND k.revoked_at IS NULL AND k.disabled_at IS NULL AND (k.expires_at IS NULL OR k.expires_at>clock_timestamp())
         AND w.disabled_at IS NULL",
        "SELECT k.governance_key_id,k.service_account_id,w.kind,w.owner_user_id FROM api_keys k
         JOIN workspaces w ON w.id=k.workspace_id WHERE k.workspace_id=$1 AND k.id=$2
         AND k.issued_to_user_id IS NOT DISTINCT FROM $3::uuid
         AND k.revoked_at IS NULL AND k.disabled_at IS NULL AND (k.expires_at IS NULL OR k.expires_at>clock_timestamp())
         AND w.disabled_at IS NULL FOR SHARE OF k,w",
    ))
    .bind(principal.workspace_id)
    .bind(principal.key_id)
    .bind(principal.user_id)
    .fetch_optional(&mut **tx)
    .await?;
    let Some((lineage, service_account, kind, owner)) = row else {
        return Ok(None);
    };
    if let Some(user) = principal.user_id {
        let active: Option<Uuid> = sqlx::query_scalar(share(
            "SELECT id FROM users WHERE id=$1 AND disabled_at IS NULL AND cleaned_at IS NULL",
            "SELECT id FROM users WHERE id=$1 AND disabled_at IS NULL AND cleaned_at IS NULL FOR SHARE",
        )).bind(user).fetch_optional(&mut **tx).await?;
        if active.is_none() {
            return Ok(None);
        }
        let roles: Vec<Uuid> = sqlx::query_scalar(share(
            "SELECT id FROM platform_role_grants WHERE user_id=$1 AND revoked_at IS NULL",
            "SELECT id FROM platform_role_grants WHERE user_id=$1 AND revoked_at IS NULL FOR SHARE",
        ))
        .bind(user)
        .fetch_all(&mut **tx)
        .await?;
        if roles.is_empty() {
            return Ok(None);
        }
        if kind == "personal" {
            return Ok((owner == Some(user)).then_some(lineage));
        }
        if !matches!(kind.as_str(), "team" | "project") {
            return Ok(None);
        }
        // Global administrative authority is deliberately NOT shared-workspace membership.
        let grants: Vec<Uuid> = sqlx::query_scalar(share(
            "SELECT id FROM workspace_membership_grants WHERE workspace_id=$1 AND user_id=$2 AND revoked_at IS NULL",
            "SELECT id FROM workspace_membership_grants WHERE workspace_id=$1 AND user_id=$2 AND revoked_at IS NULL FOR SHARE",
        )).bind(principal.workspace_id).bind(user).fetch_all(&mut **tx).await?;
        Ok((!grants.is_empty()).then_some(lineage))
    } else if matches!(kind.as_str(), "team" | "project") {
        let account: Option<Uuid> = sqlx::query_scalar(share(
            "SELECT id FROM service_accounts WHERE workspace_id=$1 AND id=$2 AND disabled_at IS NULL",
            "SELECT id FROM service_accounts WHERE workspace_id=$1 AND id=$2 AND disabled_at IS NULL FOR SHARE",
        )).bind(principal.workspace_id).bind(service_account).fetch_optional(&mut **tx).await?;
        Ok(account.map(|_| lineage))
    } else {
        Ok(None)
    }
}

#[cfg(all(test, feature = "integration-tests"))]
#[path = "auth/enterprise_tests.rs"]
mod enterprise_tests;

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
