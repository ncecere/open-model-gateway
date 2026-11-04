//! Text limits shared by the management API, sign-in, SCIM and bootstrap.

/// Text limits count characters (Unicode scalar values), as the OpenAPI
/// contract's `maxLength` and the web forms do, never UTF-8 bytes (#405).
/// O(`max`): it never walks past the limit.
pub(crate) fn chars_at_most(s: &str, max: usize) -> bool {
    s.chars().nth(max).is_none()
}
