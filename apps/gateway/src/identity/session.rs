//! Management authentication: browser session and CSRF verification, bearer
//! credential selection and admin-token requests.
use super::*;

pub(super) struct VerifiedSession {
    pub(super) principal: BrowserPrincipal,
    pub(super) token_hash: Vec<u8>,
    pub(super) csrf_hash: Vec<u8>,
    id: Uuid,
}

/// The public ID of the session a management request came in on (#79):
/// lets "Your sessions" mark it and "Sign out other sessions" keep it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CurrentSession(pub Uuid);
pub(super) async fn verify_session(
    state: &IdentityState,
    headers: &HeaderMap,
) -> Result<VerifiedSession, AuthError> {
    let token = cookie(headers, SESSION)
        .map_err(|_| AuthError(StatusCode::UNAUTHORIZED, None))?
        .ok_or(AuthError(StatusCode::UNAUTHORIZED, None))?;
    let token_hash = hash(&token);
    let row = sqlx::query_as::<_, (Uuid, String, String, Vec<u8>, Uuid, bool)>("SELECT u.id,s.verified_email,p.role,s.csrf_hash,s.id,s.last_seen_at IS NULL OR s.last_seen_at<now()-make_interval(secs=>$2) FROM browser_sessions s JOIN users u ON u.id=s.user_id JOIN effective_platform_roles p ON p.user_id=u.id WHERE s.token_hash=$1 AND s.verified_email IS NOT NULL AND s.revoked_at IS NULL AND s.expires_at>now() AND (s.idle_expires_at IS NULL OR s.idle_expires_at>now()) AND u.disabled_at IS NULL AND u.cleaned_at IS NULL")
        .bind(&token_hash).bind(f64::from(LAST_SEEN_EVERY_SECONDS)).fetch_optional(&state.store.pool).await.map_err(internal)?.ok_or(AuthError(StatusCode::UNAUTHORIZED, None))?;
    if row.5 {
        // At most one write per session per interval, not one per request.
        // Best effort: a failed write (say, a primary briefly read-only during
        // failover) must not fail an otherwise valid session.
        // Activity also moves the idle deadline, by the policy in force now;
        // an already idle-expired session is never extended (#80).
        if let Err(error) = sqlx::query("UPDATE browser_sessions SET last_seen_at=now(),idle_expires_at=now()+make_interval(secs=>$3::float8) WHERE token_hash=$1 AND revoked_at IS NULL AND expires_at>now() AND (idle_expires_at IS NULL OR idle_expires_at>now()) AND (last_seen_at IS NULL OR last_seen_at<now()-make_interval(secs=>$2))")
            .bind(&token_hash).bind(f64::from(LAST_SEEN_EVERY_SECONDS)).bind(state.policy.idle_seconds()).execute(&state.store.pool).await
        {
            tracing::warn!(%error, "could not record session activity");
        }
    }
    Ok(VerifiedSession {
        principal: BrowserPrincipal {
            user_id: row.0,
            email: row.1,
            platform_admin: row.2 == "admin",
            platform_auditor: matches!(row.2.as_str(), "admin" | "auditor"),
        },
        token_hash,
        csrf_hash: row.3,
        id: row.4,
    })
}
fn single_header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    let mut values = headers.get_all(name).iter();
    let first = values.next()?.to_str().ok()?;
    if values.next().is_some() {
        return None;
    }
    Some(first)
}
pub(super) fn verify_csrf(
    state: &IdentityState,
    headers: &HeaderMap,
    expected: &[u8],
) -> Result<(), AuthError> {
    let forbidden = || AuthError(StatusCode::FORBIDDEN, None);
    let origin = state
        .provider
        .as_ref()
        .map(|provider| provider.config.public_origin.as_str())
        .ok_or_else(forbidden)?;
    if single_header(headers, "origin") != Some(origin) {
        return Err(forbidden());
    }
    let token = single_header(headers, "x-csrf-token")
        .filter(|token| is_token(token))
        .ok_or_else(forbidden)?;
    if !bool::from(hash(token).as_slice().ct_eq(expected)) {
        return Err(forbidden());
    }
    Ok(())
}

/// A `Bearer` credential's token, from exactly one `Authorization` header
/// (the caller has already refused more than one). Strict: case-insensitive
/// `Bearer` scheme only, a non-empty token with no internal whitespace --
/// the same shape `http.rs`'s own `credential()` enforces for inference
/// keys, with no `x-api-key` fallback (management never accepts that header).
fn bearer_credential(headers: &HeaderMap) -> Option<&str> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = value.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("bearer")
        || token.is_empty()
        || token.bytes().any(|b| b.is_ascii_whitespace())
    {
        return None;
    }
    Some(token)
}

/// An inference key's exact shape (`omg_<32-hex>.<64-hex>`, 101 characters)
/// with the admin-token prefix excluded. Checked before any database access
/// so a well-formed inference key presented to a management route is
/// refused outright rather than merely failing the admin-token parser by
/// coincidence (the approved spec calls this out explicitly).
fn looks_like_inference_key(token: &str) -> bool {
    token.len() == 101 && token.starts_with("omg_") && !token.starts_with("omg_admin_")
}

/// Whether a (possibly malformed) `omg_session` cookie is present at all,
/// regardless of its value's validity: used only to refuse a request that
/// also carries an `Authorization` header (#37 PR2) -- "even an invalid
/// cookie" means this check must not itself validate the cookie's shape.
/// An undecodable `Cookie` header line is treated as present (fails closed):
/// this function only ever makes credential selection *stricter*.
fn raw_cookie_present(headers: &HeaderMap, name: &str) -> bool {
    for line in headers.get_all(header::COOKIE) {
        let Ok(line) = line.to_str() else {
            return true;
        };
        for part in line.split(';') {
            let key = part.split_once('=').map_or(part, |(k, _)| k);
            if key.trim() == name {
                return true;
            }
        }
    }
    false
}

/// Apply to every management route (not inference). Selects exactly one
/// credential family -- an admin API token (`Authorization: Bearer`) or a
/// browser session (`omg_session` cookie) -- never both, with no fallback
/// in either direction (#37 PR2): a duplicate `Authorization` header, or any
/// request carrying both an `Authorization` header and an `omg_session`
/// cookie (even an invalid one), is refused before either credential is
/// examined.
pub async fn authenticate_management(
    State(state): State<IdentityState>,
    PeerIp(peer): PeerIp,
    mut request: Request,
    next: Next,
) -> Response {
    let headers = request.headers().clone();
    let authorization_count = headers.get_all(header::AUTHORIZATION).iter().count();
    if authorization_count > 0 {
        if authorization_count > 1 || raw_cookie_present(&headers, SESSION) {
            return AuthError(StatusCode::UNAUTHORIZED, None).into_response();
        }
        return authenticate_admin_token_request(&state, peer, &headers, request, next).await;
    }
    let session = match verify_session(&state, &headers).await {
        Ok(session) => session,
        Err(error) => return error.into_response(),
    };
    if request.method() != Method::GET
        && request.method() != Method::HEAD
        && let Err(error) = verify_csrf(&state, &headers, &session.csrf_hash)
    {
        return error.into_response();
    }
    request
        .extensions_mut()
        .insert(ManagementPrincipal::Browser(session.principal.clone()));
    request.extensions_mut().insert(session.principal);
    request.extensions_mut().insert(CurrentSession(session.id));
    private(next.run(request).await)
}

/// The admin-token half of [`authenticate_management`]: a pre-authentication
/// failure (malformed credential, inference-key shape, or an unknown/
/// expired/revoked/wrong-owner token -- `authenticate_admin_token` itself
/// re-reads the primary database live, no cache) is rate-limited and
/// aggregated only, never individually audited (coordinator decision: there
/// is no token id yet to attribute it to, and auditing unauthenticated noise
/// would itself be a flood vector). Only after the token authenticates does
/// an attempt exist to audit, which `management::authorization` does for
/// every request it admits or refuses from here on.
async fn authenticate_admin_token_request(
    state: &IdentityState,
    peer: Option<IpAddr>,
    headers: &HeaderMap,
    mut request: Request,
    next: Next,
) -> Response {
    let pre_auth_fail = |reason: &'static str| {
        crate::metrics::METRICS.auth_throttled("admin_token", reason);
        AuthError(StatusCode::UNAUTHORIZED, None).into_response()
    };
    // Free, format-only rejections (not even shaped like `omg_admin_...`)
    // never consume the rate limiter below: there is nothing here for a
    // flood of these to protect (no database access, no admin-token
    // parsing), and reserving the limiter's budget for credentials that at
    // least look like an admin token keeps a mixed flood (garbage plus
    // real-looking attempts) from starving genuine admin-token traffic.
    let Some(token) = bearer_credential(headers) else {
        return pre_auth_fail("malformed_credential");
    };
    if looks_like_inference_key(token) {
        return pre_auth_fail("inference_key_shape");
    }
    let client = throttle::client_ip(peer, headers, &state.trusted_proxies);
    if let Err(wait) = state
        .admin_token_limiter
        .check(client, std::time::Instant::now())
    {
        return throttled(
            "admin_token",
            "client_rate",
            StatusCode::TOO_MANY_REQUESTS,
            wait,
        );
    }
    // Aggregate, replica-wide gate, before the expensive (one database
    // round trip) authentication lookup below (#37 PR2 finding 3): bounds
    // a distributed flood of invalid-shape or unknown-id credentials spread
    // across many client addresses, each individually under the per-IP
    // limit above, before any of that database work happens.
    if let Err(wait) = state
        .admin_token_replica_limiter
        .check(std::time::Instant::now())
    {
        return throttled(
            "admin_token",
            "replica_rate",
            StatusCode::TOO_MANY_REQUESTS,
            wait,
        );
    }
    match state.store.authenticate_admin_token(token).await {
        Ok(Some(principal)) => {
            // Per-token gate, only now that the token has successfully
            // authenticated, and before this attempt is started (audited)
            // or dispatched (#37 PR2 finding 3): the key here is the
            // token's own id, never allocated for an arbitrary
            // unauthenticated credential pre-authentication. A rejection
            // here is counted only in the fixed, low-cardinality
            // `auth_throttled` metric, never as an individual audit row --
            // the request never reaches `management::authorization`, which
            // is where that row would be written.
            if let Err(wait) = state
                .admin_token_per_token_limiter
                .check(principal.token_id, std::time::Instant::now())
            {
                return throttled(
                    "admin_token",
                    "token_rate",
                    StatusCode::TOO_MANY_REQUESTS,
                    wait,
                );
            }
            let context = AdminTokenContext {
                token_id: principal.token_id,
                owner_user_id: principal.owner_user_id,
                scopes: principal.scopes,
                ip_allowlist: principal.ip_allowlist,
                client_address: client,
            };
            request
                .extensions_mut()
                .insert(ManagementPrincipal::AdminToken(context));
            // Exempt from CSRF: the credential is a Bearer secret a
            // cross-site form or script can never already hold, unlike a
            // cookie a browser attaches automatically (#37 PR2 coordinator
            // decision). Browser-session mutations keep exact Origin and
            // CSRF-token validation above, unchanged.
            private(next.run(request).await)
        }
        Ok(None) => pre_auth_fail("invalid_credential"),
        Err(error) => {
            crate::management::log_storage_error(&error);
            storage_unavailable().into_response()
        }
    }
}
