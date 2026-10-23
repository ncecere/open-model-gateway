//! Admin API tokens (#37 PR1: storage and lifecycle only). Request
//! authentication, management routes and the UI are PR2 and PR3.
//!
//! Format `omg_admin_<32-hex uuid>.<64-hex secret>`, 256 secret bits from
//! the same fallible OS CSPRNG inference keys use (`entropy.rs`); strictly
//! parsed before any database access. The whole token's SHA-256 is stored
//! (migration 0050); lookup is by the token's embedded id (an indexed,
//! non-secret correlator, never the secret itself), then the stored digest
//! is compared to the presented token's digest in constant time — the same
//! pattern `auth.rs` uses for inference keys.
//!
//! Every authorization check re-reads the primary database: unrevoked,
//! unexpired, owner active and currently effective role exactly `admin`.
//! There is no positive authorization cache. Losing that status (manual or
//! group-derived grant loss, suspension, SCIM deactivation or cleanup)
//! permanently revokes the owner's tokens through migration 0050's
//! triggers, in the same transaction as whatever caused the loss; regaining
//! access never restores them (revocation is final).
use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};
use sqlx::{Postgres, Transaction};
use subtle::ConstantTimeEq;
use uuid::Uuid;

use crate::store::Store;

/// Every admin-token scope (route assignments are PR2's `route_policy.rs`).
/// Write scopes do not imply read scope; there is no wildcard.
pub const SCOPES: [&str; 5] = [
    "platform:read",
    "catalog:write",
    "access:write",
    "governance:write",
    "settings:write",
];

/// Shortest/longest explicit lifetime, inclusive (migration 0050 enforces
/// the same bound against `created_at` in the database).
pub const MIN_EXPIRES_IN_DAYS: i32 = 1;
pub const MAX_EXPIRES_IN_DAYS: i32 = 90;

/// Admin tokens never cache a positive authorization; successful use is
/// recorded at most this often (migration 0050: `last_used_at`/`last_used_address`).
const LAST_USE_EVERY_SECONDS: i64 = 5 * 60;

const PREFIX: &str = "omg_admin_";
const UUID_HEX_LEN: usize = 32;
const SECRET_HEX_LEN: usize = 64;
const TOKEN_LEN: usize = PREFIX.len() + UUID_HEX_LEN + 1 + SECRET_HEX_LEN;

fn digest(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}

/// Strict parse of `omg_admin_<uuid>.<secret>` before any database access:
/// exact length, exact separator position, every id/secret byte an ASCII
/// hex digit, and a valid UUID. Anything else is rejected with no query.
fn token_id(token: &str) -> Option<Uuid> {
    if token.len() != TOKEN_LEN {
        return None;
    }
    let (id, secret) = token.strip_prefix(PREFIX)?.split_once('.')?;
    if id.len() != UUID_HEX_LEN
        || secret.len() != SECRET_HEX_LEN
        || !id
            .bytes()
            .chain(secret.bytes())
            .all(|b| b.is_ascii_hexdigit())
    {
        return None;
    }
    Uuid::parse_str(id).ok()
}

/// A newly created token's plaintext is returned once, then discarded; it
/// is never stored, logged, serialized or retained. Intentionally does not
/// implement `Debug`. `created_at`/`expires_at` are the database's own
/// values (see [`create_admin_token`]), never the app's clock.
///
/// `token` is deliberately **not** zeroized on drop: it is this type's
/// entire reason to exist (the one-time plaintext the caller must still
/// return to its own caller, e.g. serialize into an HTTP response, before
/// this value is dropped), unlike [`GeneratedSecret`]'s intermediate
/// `Zeroizing` buffer, which never leaves [`GeneratedSecret::generate`].
pub struct NewAdminToken {
    pub id: Uuid,
    pub token: String,
    pub digest: [u8; 32],
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}

/// The secret-bearing part of a token, generated before any database
/// access. `created_at`/`expires_at` are assigned only after the database
/// accepts the insert (see [`create_admin_token`]), from the
/// database's own clock, never the app's.
struct GeneratedSecret {
    id: Uuid,
    token: String,
    digest: [u8; 32],
}

impl GeneratedSecret {
    /// A new secret, 256 bits from the OS CSPRNG; fails closed when the OS
    /// random number generator fails (no token is created).
    fn generate() -> Result<Self, crate::entropy::EntropyUnavailable> {
        let id = Uuid::new_v4();
        let secret = zeroize::Zeroizing::new(crate::entropy::bytes::<32>()?);
        let token = format!("{PREFIX}{}.{}", id.simple(), hex::encode(secret.as_ref()));
        let digest = digest(&token);
        Ok(Self { id, token, digest })
    }
}

/// Why [`Store::create_admin_token`] refused to create a token. Validated
/// here before any database round trip; migration 0050's `CHECK`
/// constraints are the backstop, not the primary gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NewAdminTokenError {
    /// Empty, longer than 120 characters, or containing a control character.
    InvalidName,
    /// Empty, containing an unknown scope, or containing a duplicate.
    InvalidScopes,
    /// Outside 1-90 days.
    InvalidExpiry,
    /// Supplied but empty or more than 32 entries, or any entry is not a
    /// valid `addr/len` network (unparseable address, `len` out of
    /// bounds, set host bits, or an IPv4-mapped IPv6 network).
    InvalidAllowlist,
}

#[derive(Debug)]
pub enum CreateAdminTokenError {
    Invalid(NewAdminTokenError),
    Entropy(crate::entropy::EntropyUnavailable),
    /// The owner is not, at the instant of insertion, a currently
    /// effective Platform Admin (migration 0050's `BEFORE INSERT` trigger;
    /// never bypassed, including under a concurrent demotion). Mapped from
    /// SQLSTATE `OM001` alone, never message text (coordinator review
    /// issue 5a).
    OwnerNotAdmin,
    /// The caller's transaction is running at an isolation level other
    /// than READ COMMITTED when an admin-token authority guard (creation
    /// or role-loss) ran; under REPEATABLE READ/SERIALIZABLE the guard's
    /// own snapshot can be stale behind its lock wait (coordinator review
    /// issue 1), so it refuses outright instead. Mapped from SQLSTATE
    /// `OM002` alone. The gateway's own transactions are always READ
    /// COMMITTED (`crate::db::begin`), so this should never occur on a
    /// production path; it exists as a hard backstop.
    UnsupportedIsolation,
    Database(sqlx::Error),
}
impl std::fmt::Display for CreateAdminTokenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(e) => write!(f, "invalid admin token input: {e:?}"),
            Self::Entropy(e) => e.fmt(f),
            Self::OwnerNotAdmin => {
                f.write_str("the owner is not a currently effective Platform Admin")
            }
            Self::UnsupportedIsolation => {
                f.write_str("admin token authority checks require a READ COMMITTED transaction")
            }
            Self::Database(e) => e.fmt(f),
        }
    }
}
impl std::error::Error for CreateAdminTokenError {}

/// Map a database error from the creation `INSERT`. The owner-admin guard
/// trigger (migration 0050) raises its own dedicated SQLSTATE (`OM001`),
/// as does the isolation-level guard (`OM002`); both are mapped on the
/// code alone, with no message-text matching, so a translated or reworded
/// `RAISE EXCEPTION` message can never silently stop being recognized.
/// Everything else is passed through as [`CreateAdminTokenError::Database`].
fn map_create_error(error: sqlx::Error) -> CreateAdminTokenError {
    if let sqlx::Error::Database(ref db) = error {
        match db.code().as_deref() {
            Some("OM001") => return CreateAdminTokenError::OwnerNotAdmin,
            Some("OM002") => return CreateAdminTokenError::UnsupportedIsolation,
            _ => {}
        }
    }
    CreateAdminTokenError::Database(error)
}

fn valid_name(name: &str) -> bool {
    let len = name.chars().count();
    (1..=120).contains(&len) && !name.chars().any(|c| c.is_control())
}

fn valid_scopes(scopes: &[String]) -> bool {
    if scopes.is_empty() || scopes.len() > SCOPES.len() {
        return false;
    }
    if !scopes.iter().all(|s| SCOPES.contains(&s.as_str())) {
        return false;
    }
    let mut sorted: Vec<&str> = scopes.iter().map(String::as_str).collect();
    sorted.sort_unstable();
    sorted.dedup();
    sorted.len() == scopes.len()
}

fn valid_expires_in_days(days: i32) -> bool {
    (MIN_EXPIRES_IN_DAYS..=MAX_EXPIRES_IN_DAYS).contains(&days)
}

/// Validate and canonicalize an allowlist before any database round trip
/// (coordinator review issue 5b): 1-32 entries, each `addr/len` (or a bare
/// address), a parseable IP, `len<=32` (IPv4) or `len<=128` (IPv6), no
/// host bits set and no IPv4-mapped IPv6 network (write it as plain IPv4
/// instead) -- the same rules `egress::parse_network` already enforces
/// for `GATEWAY_EGRESS_PRIVATE_ALLOW`, reused here so both apply exactly
/// the same checks. PostgreSQL's own `cidr` type would reject most of
/// these too, but as a raw, untyped `22P02` error; checking here first
/// turns a bad entry into `NewAdminTokenError::InvalidAllowlist` with no
/// database access at all, and normalizes every entry to its canonical
/// `addr/len` string (e.g. uppercase hex, or a bare address without a
/// prefix) before it is ever stored.
fn canonical_allowlist(list: &[String]) -> Option<Vec<String>> {
    if !(1..=32).contains(&list.len()) {
        return None;
    }
    let canonical: Vec<String> = list
        .iter()
        .map(|entry| {
            let (addr, prefix) = crate::egress::parse_network("ip_allowlist", entry).ok()?;
            // A zero-length prefix (`0.0.0.0/0` or `::/0`) stands for "every
            // address": accepting it here would let a caller believe a
            // token is IP-restricted when it is not (adversarial
            // re-review of #37 PR1, issue 4). That's already spelled by
            // omitting the allowlist entirely, so reject it as malformed
            // input rather than silently accept an unrestricted entry
            // inside what is supposed to be a restriction.
            if prefix == 0 {
                return None;
            }
            Some(format!("{addr}/{prefix}"))
        })
        .collect::<Option<_>>()?;
    // Reject duplicates (after canonicalization), rather than silently
    // deduplicating: an operator who listed the same network twice --
    // including two differently-represented forms of it, like
    // `10.0.0.0/24` and `010.000.000.000/24` -- almost certainly made a
    // mistake, and silently accepting it would hide that.
    let mut sorted = canonical.clone();
    sorted.sort_unstable();
    sorted.dedup();
    if sorted.len() != canonical.len() {
        return None;
    }
    Some(canonical)
}

/// A lookup-for-authorization hit: live, unrevoked, unexpired, owner active
/// and currently effective role exactly `admin` (checked by the query
/// itself, not cached). Route-scope enforcement is PR2's job.
///
/// `ip_allowlist` is the token's canonical `addr/len` allowlist entries
/// (empty/`None` means unrestricted), exactly as stored (coordinator/
/// adversarial re-review of #37 PR1, issue 5): **PR2 must check the
/// request's resolved client address against it before dispatching any
/// request authenticated by this principal.** This PR only stores and
/// returns it; nothing here enforces it yet.
pub struct AdminTokenPrincipal {
    pub token_id: Uuid,
    pub owner_user_id: Uuid,
    pub scopes: Vec<String>,
    pub ip_allowlist: Option<Vec<String>>,
}

#[derive(sqlx::FromRow)]
struct TokenRecord {
    id: Uuid,
    owner_user_id: Uuid,
    secret_hash: Vec<u8>,
    scopes: Vec<String>,
    ip_allowlist: Option<Vec<String>>,
}

/// Create a token owned by `owner_user_id` (the caller, a currently
/// effective Platform Admin; enforcing that this is the *creating* user is
/// PR2's handler-level job — this is the storage primitive). Returns the
/// plaintext once.
///
/// Runs on the caller's own `tx`, never a separate pool connection
/// (coordinator review of #37 PR1, issue 2: PR1 originally ran this on
/// `&self.pool`. Migration 0050's `BEFORE INSERT` trigger takes the
/// owner's `governance::locks` user-scope authority lock
/// (`Scope::User(owner_user_id)`) before checking that the owner is
/// currently an effective Admin; that lock is transaction-scoped
/// (`pg_advisory_xact_lock`), so a caller that already holds it in its own
/// transaction — as PR2's handler is documented to, before calling this —
/// and then ran this `INSERT` on a *different* connection would hang: the
/// `INSERT`'s own re-entrant acquisition attempt blocks until the caller's
/// transaction ends, which it cannot do until this call returns). **The
/// caller must still take that same lock exclusive up front in `tx`**,
/// like every other management change of that scope (AGENTS.md's
/// canonical lock order), before calling this: the trigger's own
/// re-entrant acquisition inside the single `INSERT` statement is a
/// correctness backstop (and, now that both run on the same connection,
/// free to ask for again), not a substitute for the caller holding the
/// lock across its own multi-statement transaction (e.g. an earlier read
/// of the caller's own role, or audit insertion, that must not race a
/// concurrent demotion either).
pub async fn create_admin_token(
    tx: &mut Transaction<'_, Postgres>,
    owner_user_id: Uuid,
    name: &str,
    scopes: &[String],
    expires_in_days: i32,
    ip_allowlist: Option<&[String]>,
) -> Result<NewAdminToken, CreateAdminTokenError> {
    if !valid_name(name) {
        return Err(CreateAdminTokenError::Invalid(
            NewAdminTokenError::InvalidName,
        ));
    }
    if !valid_scopes(scopes) {
        return Err(CreateAdminTokenError::Invalid(
            NewAdminTokenError::InvalidScopes,
        ));
    }
    if !valid_expires_in_days(expires_in_days) {
        return Err(CreateAdminTokenError::Invalid(
            NewAdminTokenError::InvalidExpiry,
        ));
    }
    let canonical_allowlist = match ip_allowlist {
        None => None,
        Some(list) => Some(
            canonical_allowlist(list).ok_or(CreateAdminTokenError::Invalid(
                NewAdminTokenError::InvalidAllowlist,
            ))?,
        ),
    };
    let secret = GeneratedSecret::generate().map_err(CreateAdminTokenError::Entropy)?;
    // created_at and expires_at are computed by the database, from one
    // instant (`now()` is fixed for the whole transaction, so the two
    // references below are identical), never the app's clock: a
    // skewed app clock must not be able to create an already-expired
    // token or one that outlives 90 real days.
    let (created_at, expires_at): (DateTime<Utc>, DateTime<Utc>) = sqlx::query_as(
        "INSERT INTO admin_api_tokens(id,owner_user_id,name,secret_hash,scopes,ip_allowlist,created_at,expires_at)
         VALUES($1,$2,$3,$4,$5,$6::text[]::cidr[],now(),now()+make_interval(days=>$7))
         RETURNING created_at,expires_at",
    )
    .bind(secret.id)
    .bind(owner_user_id)
    .bind(name)
    .bind(secret.digest.as_slice())
    .bind(scopes)
    .bind(canonical_allowlist.as_deref())
    .bind(expires_in_days)
    .fetch_one(&mut **tx)
    .await
    .map_err(map_create_error)?;
    Ok(NewAdminToken {
        id: secret.id,
        token: secret.token,
        digest: secret.digest,
        created_at,
        expires_at,
    })
}

/// Idempotent: revoking an already-revoked (or missing) token is not an
/// error. `Ok(true)` iff a token with this id exists (revoked now or
/// already). Revocation is final (migration 0050's trigger): nothing
/// ever un-revokes a token, including regaining the Admin role.
///
/// Runs on the caller's own `tx`, like [`create_admin_token`]: revocation
/// must be atomic with whatever else that transaction does (PR2's audit
/// insertion, in particular), and roll back with it rather than
/// committing early on a separate connection the instant this is called.
/// Revoking a token does not itself need the owner's scope lock (no
/// trigger on `admin_api_tokens` takes one), but running on the caller's
/// transaction keeps this symmetric with `create_admin_token` and correct
/// for PR2's atomicity requirement regardless.
pub async fn revoke_admin_token(
    tx: &mut Transaction<'_, Postgres>,
    id: Uuid,
) -> Result<bool, sqlx::Error> {
    let result = sqlx::query(
        "UPDATE admin_api_tokens SET revoked_at=coalesce(revoked_at,now()) WHERE id=$1",
    )
    .bind(id)
    .execute(&mut **tx)
    .await?;
    Ok(result.rows_affected() > 0)
}

impl Store {
    /// Live lookup-for-authorization. A malformed token is rejected by the
    /// strict parser with no database access at all. No positive
    /// authorization cache: every call re-reads the primary database.
    pub async fn authenticate_admin_token(
        &self,
        token: &str,
    ) -> Result<Option<AdminTokenPrincipal>, sqlx::Error> {
        let Some(id) = token_id(token) else {
            return Ok(None);
        };
        let record: Option<TokenRecord> = sqlx::query_as(
            "SELECT t.id,t.owner_user_id,t.secret_hash,t.scopes,t.ip_allowlist::text[] AS ip_allowlist FROM admin_api_tokens t
             JOIN effective_platform_roles r ON r.user_id=t.owner_user_id AND r.role='admin'
             WHERE t.id=$1 AND t.revoked_at IS NULL AND t.expires_at>clock_timestamp()",
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
        Ok(Some(AdminTokenPrincipal {
            token_id: record.id,
            owner_user_id: record.owner_user_id,
            scopes: record.scopes,
            ip_allowlist: record.ip_allowlist,
        }))
    }

    /// Record successful use, throttled to once per 5 minutes across
    /// replicas via one conditional `UPDATE` (no read-then-write race): a
    /// second call inside the window changes nothing. `address` is the
    /// resolved client address (PR2's job to resolve it); absent here, it
    /// is stored as `NULL`.
    ///
    /// Takes a parsed [`std::net::IpAddr`], not a string (coordinator
    /// review issue 5c): the caller has necessarily already resolved and
    /// validated an address object (identity.rs/throttle.rs's job, per
    /// this module's own doc comment) by the time it gets here, so this
    /// signature cannot accept an arbitrary unparsed string and defer
    /// that validation to PostgreSQL's `inet` cast.
    pub async fn touch_admin_token_use(
        &self,
        id: Uuid,
        address: Option<std::net::IpAddr>,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE admin_api_tokens SET last_used_at=clock_timestamp(),last_used_address=$2::text::inet
             WHERE id=$1 AND (last_used_at IS NULL OR last_used_at<clock_timestamp()-make_interval(secs=>$3))",
        )
        .bind(id)
        .bind(address.map(|a| a.to_string()))
        .bind(LAST_USE_EVERY_SECONDS as f64)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}

#[cfg(all(test, feature = "integration-tests"))]
#[path = "admin_tokens/enterprise_tests.rs"]
mod enterprise_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_have_the_expected_format_and_independent_random_secrets() {
        let first = GeneratedSecret::generate().unwrap();
        let second = GeneratedSecret::generate().unwrap();
        assert_eq!(first.token.len(), TOKEN_LEN);
        assert!(first.token.starts_with(PREFIX));
        assert_eq!(token_id(&first.token), Some(first.id));
        assert_eq!(first.digest, digest(&first.token));
        assert_ne!(first.token, second.token);
        assert_ne!(first.digest, second.digest);
    }

    #[test]
    fn the_parser_rejects_malformed_tokens_before_any_lookup() {
        let good = GeneratedSecret::generate().unwrap();
        for bad in [
            "",
            "omg_admin_",
            &good.token[..good.token.len() - 1],
            &format!("{} ", good.token),
            &good.token.replace("omg_admin_", "omg_"),
            &good.token.replace('.', ""),
            &good.token.replacen('.', "..", 1),
            &good.token.to_uppercase(),
            &good.token.replace('a', "g"),
            &format!("omg_admin_{}.{}", "0".repeat(32), "z".repeat(64)),
        ] {
            assert_eq!(token_id(bad), None, "{bad:?}");
        }
        // A well-formed id with a tampered secret still parses (the digest
        // comparison, not the parser, is what refuses a wrong secret).
        let tampered = format!("{}0", &good.token[..good.token.len() - 1]);
        assert_eq!(token_id(&tampered), Some(good.id));
    }

    #[test]
    fn scopes_are_validated_non_empty_known_and_without_duplicates() {
        let s = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(valid_scopes(&s(&["platform:read"])));
        assert!(valid_scopes(&s(&SCOPES)));
        assert!(!valid_scopes(&s(&[])));
        assert!(!valid_scopes(&s(&["platform:admin"])));
        assert!(!valid_scopes(&s(&["platform:read", "platform:read"])));
    }

    #[test]
    fn expiry_days_are_bounded_one_to_ninety() {
        assert!(!valid_expires_in_days(0));
        assert!(valid_expires_in_days(1));
        assert!(valid_expires_in_days(30));
        assert!(valid_expires_in_days(90));
        assert!(!valid_expires_in_days(91));
        assert!(!valid_expires_in_days(-1));
    }

    #[test]
    fn allowlist_entries_are_bounded_one_to_thirty_two() {
        // Distinct networks (adversarial re-review of #37 PR1, issue 4:
        // duplicates are now rejected on their own, so the cardinality
        // bound needs `count` genuinely different entries to isolate it).
        let n = |count: usize| {
            (0..count)
                .map(|i| format!("10.0.{i}.0/24"))
                .collect::<Vec<_>>()
        };
        assert!(canonical_allowlist(&n(0)).is_none());
        assert!(canonical_allowlist(&n(1)).is_some());
        assert!(canonical_allowlist(&n(32)).is_some());
        assert!(canonical_allowlist(&n(33)).is_none());
    }

    #[test]
    fn allowlist_entries_are_validated_and_canonicalized() {
        for bad in [
            "not-an-ip",
            "10.0.0.1/33",
            "10.0.0.5/24",
            "2001:db8::/129",
            "::ffff:10.0.0.0/120",
        ] {
            assert!(
                canonical_allowlist(&[bad.to_string()]).is_none(),
                "{bad:?} should be rejected"
            );
        }
        assert_eq!(
            canonical_allowlist(&["2001:DB8::/32".to_string(), "10.0.0.0".to_string()]),
            Some(vec!["2001:db8::/32".to_string(), "10.0.0.0/32".to_string()])
        );
    }

    /// Adversarial re-review of #37 PR1, issue 4: `0.0.0.0/0`/`::/0` would
    /// stand for "every address", silently defeating the whole point of
    /// supplying an allowlist (and is indistinguishable from not
    /// restricting the token at all, which is already spelled by omitting
    /// the field). Rejected outright, for both address families, alone or
    /// alongside another entry.
    #[test]
    fn allowlist_rejects_a_zero_length_prefix_in_either_family() {
        for bad in ["0.0.0.0/0", "::/0"] {
            assert!(
                canonical_allowlist(&[bad.to_string()]).is_none(),
                "{bad:?} should be rejected"
            );
            assert!(
                canonical_allowlist(&["10.0.0.0/24".to_string(), bad.to_string()]).is_none(),
                "{bad:?} should be rejected even alongside a legitimate entry"
            );
        }
        // A bare address (implicit host prefix, /32 or /128) is unaffected.
        assert!(canonical_allowlist(&["0.0.0.0".to_string()]).is_some());
    }

    /// Adversarial re-review of #37 PR1, issue 4: a duplicate entry,
    /// including two differently-represented forms of the same network,
    /// is rejected rather than silently collapsed.
    #[test]
    fn allowlist_rejects_duplicate_entries_after_canonicalization() {
        assert!(
            canonical_allowlist(&["10.0.0.0/24".to_string(), "10.0.0.0/24".to_string()]).is_none()
        );
        // Same network, different representation (case, explicit host prefix).
        assert!(
            canonical_allowlist(&["2001:DB8::/32".to_string(), "2001:db8::/32".to_string()])
                .is_none()
        );
        assert!(
            canonical_allowlist(&["10.0.0.5".to_string(), "10.0.0.5/32".to_string()]).is_none(),
            "a bare address and its explicit /32 form are the same network"
        );
    }

    #[test]
    fn invalid_names_are_rejected() {
        assert!(valid_name("My token"));
        assert!(!valid_name(""));
        assert!(!valid_name(&"x".repeat(121)));
        assert!(valid_name(&"x".repeat(120)));
        assert!(!valid_name("bad\ttab"));
    }
}
