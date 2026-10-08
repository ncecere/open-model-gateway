//! Optional client-supplied request labels: a session id and an app name.
//!
//! These are untrusted metadata for grouping and display in Logs, never used
//! for authorization, tenancy or attribution, and never forwarded upstream.
//! Sources, in precedence order:
//! - session: the `X-Session-Id` header; else the protocol body's
//!   `metadata.session_id` or `user` (OpenAI) or `metadata.user_id` (Anthropic);
//! - app: the `X-Title` header (as OpenRouter clients send it).
//!
//! Values that are missing or invalid (empty, too long, control characters,
//! surrounding whitespace, not UTF-8) are simply not recorded; they never fail
//! an inference request.
//!
//! The labels travel to admission through a task-local scope set by the
//! protocol handlers, so every attempt of one root request (including
//! failover attempts) records the same values.
use std::future::Future;

use axum::http::HeaderMap;

pub const MAX_SESSION_CHARS: usize = 128;
pub const MAX_APP_CHARS: usize = 200;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ClientMetadata {
    pub session_id: Option<String>,
    pub app: Option<String>,
}

tokio::task_local! {
    static CLIENT: ClientMetadata;
}

/// A bounded printable label: 1..=max characters, no control characters and
/// no leading/trailing whitespace. The database enforces the same rule.
pub fn valid_label(value: &str, max: usize) -> bool {
    let chars = value.chars().count();
    (1..=max).contains(&chars) && !value.chars().any(char::is_control) && value.trim() == value
}
pub fn valid_session_id(value: &str) -> bool {
    valid_label(value, MAX_SESSION_CHARS)
}
fn header(headers: &HeaderMap, name: &str, max: usize) -> Option<String> {
    let mut values = headers.get_all(name).iter();
    let value = values.next()?;
    if values.next().is_some() {
        return None;
    }
    let value = std::str::from_utf8(value.as_bytes()).ok()?;
    valid_label(value, max).then(|| value.to_owned())
}

impl ClientMetadata {
    /// Header-derived labels (`X-Session-Id`, `X-Title`). Repeated headers are ignored.
    pub fn from_headers(headers: &HeaderMap) -> Self {
        Self {
            session_id: header(headers, "x-session-id", MAX_SESSION_CHARS),
            app: header(headers, "x-title", MAX_APP_CHARS),
        }
    }
    /// Use the first valid body candidate when no header session was given.
    pub fn with_body_session<'a>(
        mut self,
        candidates: impl IntoIterator<Item = Option<&'a str>>,
    ) -> Self {
        if self.session_id.is_none() {
            self.session_id = candidates
                .into_iter()
                .flatten()
                .find(|v| valid_session_id(v))
                .map(str::to_owned);
        }
        self
    }
    /// Run `future` with these labels visible to admission.
    pub async fn scope<F: Future>(self, future: F) -> F::Output {
        CLIENT.scope(self, future).await
    }
}

/// OpenAI request `metadata`: at most 16 string pairs, keys 1-64 and values up
/// to 512 characters (OpenAI's documented bounds). Recorded only as a session
/// label source (`session_id`); never stored whole or forwarded upstream.
pub type OpenAiMetadata = std::collections::BTreeMap<String, String>;
pub fn valid_openai_metadata(metadata: Option<&OpenAiMetadata>) -> bool {
    metadata.is_none_or(|m| {
        m.len() <= 16
            && m.iter()
                .all(|(k, v)| (1..=64).contains(&k.chars().count()) && v.chars().count() <= 512)
    })
}
/// Session candidates of an OpenAI body: `metadata.session_id`, then `user`.
pub fn openai_session<'a>(
    metadata: Option<&'a OpenAiMetadata>,
    user: Option<&'a str>,
) -> [Option<&'a str>; 2] {
    [
        metadata
            .and_then(|m| m.get("session_id"))
            .map(String::as_str),
        user,
    ]
}

/// The labels of the current request, or none outside a scope.
pub fn current() -> ClientMetadata {
    CLIENT.try_with(Clone::clone).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn labels_are_bounded_printable_and_trimmed() {
        assert!(valid_session_id("abc"));
        assert!(valid_session_id("conversation 42/α"));
        assert!(valid_session_id(&"x".repeat(128)));
        for bad in ["", " a", "a ", "a\nb", "a\u{7f}", &"x".repeat(129)] {
            assert!(!valid_session_id(bad), "{bad:?}");
        }
    }

    #[test]
    fn header_precedence_and_invalid_values_are_dropped() {
        let mut h = HeaderMap::new();
        h.insert("x-session-id", HeaderValue::from_static("sess-1"));
        h.insert("x-title", HeaderValue::from_static("My App"));
        let m = ClientMetadata::from_headers(&h).with_body_session([Some("body")]);
        assert_eq!(m.session_id.as_deref(), Some("sess-1"));
        assert_eq!(m.app.as_deref(), Some("My App"));
        let m = ClientMetadata::default().with_body_session([None, Some(" bad"), Some("user-7")]);
        assert_eq!(m.session_id.as_deref(), Some("user-7"));
        let mut h = HeaderMap::new();
        h.append("x-session-id", HeaderValue::from_static("a"));
        h.append("x-session-id", HeaderValue::from_static("b"));
        h.insert("x-title", HeaderValue::from_bytes(&[0xff]).unwrap());
        assert_eq!(ClientMetadata::from_headers(&h), ClientMetadata::default());
    }

    #[tokio::test]
    async fn scope_is_visible_only_inside() {
        assert_eq!(current(), ClientMetadata::default());
        let m = ClientMetadata {
            session_id: Some("s".into()),
            app: None,
        };
        let seen = m.clone().scope(async { current() }).await;
        assert_eq!(seen, m);
    }
}
