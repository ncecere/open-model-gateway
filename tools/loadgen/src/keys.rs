//! Deterministic load-test inference keys.
//!
//! The seeder (`seed.sql`) and the generator derive the same tokens from a
//! seed string and a key index, so no token is ever printed, stored or passed
//! between processes. They are credentials only for a throwaway
//! `omg_loadtest*` database; the seeder refuses any other database.
use sha2::{Digest, Sha256};

fn sha256_hex(input: &str) -> String {
    hex::encode(Sha256::digest(input.as_bytes()))
}

/// Key id (32 lowercase hex characters; a UUID without hyphens).
pub fn key_id_hex(seed: &str, index: u64) -> String {
    sha256_hex(&format!("{seed}:key:{index}"))[..32].to_owned()
}

/// The inference token `omg_<id>.<64 hex>` of key `index` (0-based).
pub fn token(seed: &str, index: u64) -> String {
    format!(
        "omg_{}.{}",
        key_id_hex(seed, index),
        sha256_hex(&format!("{seed}:secret:{index}"))
    )
}

/// SHA-256 of the token, as stored in `api_keys.secret_hash`.
pub fn secret_hash(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_have_the_gateway_format_and_are_deterministic() {
        let t = token("s1", 0);
        assert_eq!(t.len(), 101);
        assert!(t.starts_with("omg_"));
        let (id, secret) = t[4..].split_once('.').unwrap();
        assert_eq!(id.len(), 32);
        assert_eq!(secret.len(), 64);
        assert!(uuid::Uuid::parse_str(id).is_ok());
        assert_eq!(t, token("s1", 0));
        assert_ne!(t, token("s1", 1));
        assert_ne!(t, token("s2", 0));
        // Same derivation as seed.sql:
        // left(encode(sha256(convert_to('s1:key:0','UTF8')),'hex'),32)
        assert_eq!(
            key_id_hex("s1", 0),
            &hex::encode(Sha256::digest(b"s1:key:0"))[..32]
        );
        assert_eq!(
            secret_hash(&t),
            <[u8; 32]>::from(Sha256::digest(t.as_bytes()))
        );
    }
}
