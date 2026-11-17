//! Compare callback state without disclosing either value to diagnostics.
use oauth2::CsrfToken;
use sha2::{Digest, Sha256};

pub(crate) fn matches(actual: &CsrfToken, expected: &CsrfToken) -> bool {
    // Use the SDK's timing-resistant secret-trait algorithm in every feature set.
    !expected.secret().is_empty()
        && Sha256::digest(actual.secret()) == Sha256::digest(expected.secret())
}
