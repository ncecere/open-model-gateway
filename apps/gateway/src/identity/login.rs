//! Sign-in: the public configuration, login start, the callback with identity
//! resolution, the denied-callback page and logout.
use super::*;

/// Public sign-in configuration: whether SSO is configured and the uploaded
/// installation logo (`{url, updated_at}` or null) with the installation name
/// as its alt text (only when a logo is set). Both are null on a storage
/// error, so the sign-in page never depends on them.
pub(super) async fn auth_config(
    State(state): State<IdentityState>,
    files: Option<axum::Extension<crate::filestore::FileStoreRuntime>>,
) -> Response {
    let files = files.map(|axum::Extension(r)| r).unwrap_or_default();
    let (logo, name) = crate::management::branding::sign_in_branding(&state.store.pool, &files)
        .await
        .unwrap_or_default();
    private(
        Json(serde_json::json!({
            "enabled": state.provider.is_some(),
            "logo": logo,
            "installation_name": name,
        }))
        .into_response(),
    )
}

#[derive(Deserialize, Default)]
pub(super) struct LoginQuery {
    return_to: Option<String>,
}

pub(super) async fn login(
    State(state): State<IdentityState>,
    PeerIp(peer): PeerIp,
    headers: HeaderMap,
    query: Result<Query<LoginQuery>, QueryRejection>,
) -> Result<Response, AuthError> {
    let provider = state
        .provider
        .as_ref()
        .ok_or(AuthError(StatusCode::SERVICE_UNAVAILABLE, None))?;
    let client = throttle::client_ip(peer, &headers, &provider.config.trusted_proxies);
    if let Err(wait) = provider
        .login_limiter
        .check(client, std::time::Instant::now())
    {
        return Ok(throttled(
            "login",
            "client_rate",
            StatusCode::TOO_MANY_REQUESTS,
            wait,
        ));
    }
    let full = || {
        provider
            .login_full_until
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    };
    if full().is_some_and(|until| std::time::Instant::now() < until) {
        return Ok(throttled(
            "login",
            "pending_cap",
            StatusCode::SERVICE_UNAVAILABLE,
            Duration::from_secs(60),
        ));
    }
    // An unsafe or malformed return path is dropped (sign-in still works and lands on Home).
    let return_to = query
        .ok()
        .and_then(|Query(q)| q.return_to)
        .as_deref()
        .and_then(safe_return_path)
        .filter(|p| p != "/");
    // Every secret first, so an RNG failure stores and redirects nothing. The
    // PKCE verifier is 32 OS CSPRNG bytes as well (RFC 7636 base64url).
    let (state_token, nonce_token, browser) = (random_token()?, random_token()?, random_token()?);
    let verifier = {
        use base64::Engine as _;
        let bytes = crate::entropy::bytes::<32>()
            .map_err(|_| AuthError(StatusCode::INTERNAL_SERVER_ERROR, None))?;
        PkceCodeVerifier::new(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
    };
    let challenge = PkceCodeChallenge::from_code_verifier_sha256(&verifier);
    let (url, oauth_state, nonce) = provider
        .client
        .authorize_url(
            CoreAuthenticationFlow::AuthorizationCode,
            move || CsrfToken::new(state_token),
            move || Nonce::new(nonce_token),
        )
        .add_scope(Scope::new("email".into()))
        // `profile` carries the optional display name; it is presentation only.
        .add_scope(Scope::new("profile".into()))
        .set_pkce_challenge(challenge)
        .url();
    // Installation-wide bound on pending attempts: past it nothing is stored.
    // The count stops at the cap, so it costs at most `cap` index entries.
    // Concurrent inserts don't see each other (READ COMMITTED), so the cap can
    // be exceeded by the inserts in flight at once (at most one per database
    // connection); that still bounds the table. A strict cap would need a lock
    // every sign-in waits on.
    let stored = sqlx::query("INSERT INTO oidc_login_attempts(state_hash,browser_hash,nonce,pkce_verifier,expires_at,return_to) SELECT $1,$2,$3,$4,now()+interval '10 minutes',$5 WHERE (SELECT count(*) FROM (SELECT 1 FROM oidc_login_attempts WHERE expires_at>now() LIMIT $6) pending)<$6")
        .bind(hash(oauth_state.secret())).bind(hash(&browser)).bind(nonce.secret()).bind(verifier.secret()).bind(return_to).bind(provider.config.pending_login_cap)
        .execute(&state.store.pool).await.map_err(internal)?;
    if stored.rows_affected() == 0 {
        *full() = Some(std::time::Instant::now() + provider.config.pending_cap_latch);
        return Ok(throttled(
            "login",
            "pending_cap",
            StatusCode::SERVICE_UNAVAILABLE,
            Duration::from_secs(60),
        ));
    }
    let mut response = private(Redirect::to(url.as_str()).into_response());
    set_cookie(
        &mut response,
        BROWSER,
        &browser,
        LOGIN_SECONDS,
        provider.config.secure_cookies,
        true,
    );
    Ok(response)
}

/// The connection's peer address, when the server records it
/// (`into_make_service_with_connect_info`); in-process tests have none.
pub struct PeerIp(pub(super) Option<IpAddr>);
impl<S: Send + Sync> axum::extract::FromRequestParts<S> for PeerIp {
    type Rejection = std::convert::Infallible;
    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        _: &S,
    ) -> Result<Self, Self::Rejection> {
        Ok(Self(
            parts
                .extensions
                .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
                .map(|info| info.0.ip()),
        ))
    }
}

/// A throttled sign-in step: nothing stored, no cookie, a `Retry-After`, and
/// a counter labelled only by step and reason (never an address or identity).
pub(super) fn throttled(
    endpoint: &'static str,
    reason: &'static str,
    status: StatusCode,
    wait: Duration,
) -> Response {
    crate::metrics::METRICS.auth_throttled(endpoint, reason);
    let mut response = private(
        (
            status,
            Json(serde_json::json!({"error": "Too many sign-in attempts; try again shortly"})),
        )
            .into_response(),
    );
    if let Ok(value) = wait.as_secs().max(1).to_string().parse() {
        response.headers_mut().insert(header::RETRY_AFTER, value);
    }
    response
}

#[derive(Deserialize)]
pub(super) struct CallbackQuery {
    pub(super) state: String,
    pub(super) code: Option<String>,
    pub(super) error: Option<String>,
}

pub(super) async fn consume_attempt(
    store: &Store,
    oauth_state: &str,
    browser: &str,
) -> Result<(String, String, Option<String>), AuthError> {
    let mut tx = crate::db::begin(&store.pool).await.map_err(internal)?;
    let attempt = sqlx::query_as::<_, (String, String, Option<String>)>("DELETE FROM oidc_login_attempts WHERE state_hash=$1 AND browser_hash=$2 AND expires_at>now() RETURNING nonce,pkce_verifier,return_to")
        .bind(hash(oauth_state)).bind(hash(browser)).fetch_optional(&mut *tx).await.map_err(internal)?;
    // Commit before ANY token endpoint request: replay is impossible even if exchange fails.
    tx.commit().await.map_err(internal)?;
    attempt.ok_or_else(invalid)
}

#[cfg_attr(not(test), expect(clippy::too_many_lines, reason = "#499"))]
pub(super) async fn callback(
    State(state): State<IdentityState>,
    PeerIp(peer): PeerIp,
    headers: HeaderMap,
    query: Result<Query<CallbackQuery>, QueryRejection>,
) -> Result<Response, AuthError> {
    let provider = state
        .provider
        .as_ref()
        .ok_or(AuthError(StatusCode::SERVICE_UNAVAILABLE, None))?;
    // Before the attempt is consumed: a throttled callback leaves it usable.
    let client = throttle::client_ip(peer, &headers, &provider.config.trusted_proxies);
    if let Err(wait) = provider
        .callback_limiter
        .check(client, std::time::Instant::now())
    {
        return Ok(throttled(
            "callback",
            "client_rate",
            StatusCode::TOO_MANY_REQUESTS,
            wait,
        ));
    }
    let Query(query) = query.map_err(|_| invalid())?;
    if !is_token(&query.state) {
        return Err(invalid());
    }
    let browser = cookie(&headers, BROWSER)?.ok_or_else(invalid)?;
    let (nonce, verifier, return_to) =
        consume_attempt(&state.store, &query.state, &browser).await?;
    if query.error.is_some() {
        return Err(invalid());
    }
    let code = query
        .code
        .filter(|code| !code.is_empty() && crate::text::chars_at_most(code, 8192))
        .ok_or_else(invalid)?;
    let transport = |request| provider.http.clone().send(request);
    let tokens = provider
        .client
        .exchange_code(AuthorizationCode::new(code))
        .map_err(|_| invalid())?
        .set_pkce_verifier(PkceCodeVerifier::new(verifier))
        .request_async(&transport)
        .await
        .map_err(|_| invalid())?;
    let id_token = tokens.id_token().ok_or_else(invalid)?;
    // Signature (cached, rotating JWKS; asymmetric algorithms only), issuer, audience,
    // expiration and the original nonce.
    let claims = verify_id_token(provider, id_token, &Nonce::new(nonce)).await?;
    // The library deliberately leaves `azp` policy to relying parties.
    if claims
        .authorized_party()
        .is_some_and(|party| party.as_str() != provider.config.client_id)
        || (claims.audiences().len() > 1 && claims.authorized_party().is_none())
        || claims.email_verified() != Some(true)
    {
        return Err(invalid());
    }
    let email = claims
        .email()
        .map(|email| email.as_str().to_lowercase())
        .filter(|email| sign_in_email_valid(email))
        .ok_or_else(invalid)?;
    let subject = claims.subject().as_str();
    if subject.is_empty() || subject.len() > 2048 || subject.chars().any(char::is_control) {
        return Err(invalid());
    }
    let groups = claims
        .additional_claims()
        .groups(&provider.config.groups_claim)
        .ok_or(AuthError(StatusCode::FORBIDDEN, None))?;
    // Presentation only: the verified token's display name (or none) replaces the stored one.
    let display_name = claims
        .name()
        .and_then(|name| name.get(None))
        .and_then(|name| display_name(name.as_str()));
    let user_id = match resolve_identity_with(
        &state.store,
        provider.config.issuer.as_str(),
        subject,
        &email,
        &groups,
        state.scim.is_some(),
    )
    .await
    {
        Ok(id) => id,
        Err(AuthError(StatusCode::FORBIDDEN, _)) => {
            // A denied sign-in (no platform role) still records who tried under the
            // linked identity, so Admin › Users shows a name rather than a bare row.
            sqlx::query("UPDATE users u SET display_name=$3 FROM oidc_identities i WHERE i.issuer=$1 AND i.subject=$2 AND u.id=i.user_id AND u.cleaned_at IS NULL AND u.display_name IS DISTINCT FROM $3")
                .bind(provider.config.issuer.as_str())
                .bind(subject)
                .bind(&display_name)
                .execute(&state.store.pool)
                .await
                .map_err(internal)?;
            if wants_html(&headers) {
                return denied_browser_callback(&state, &headers, provider.config.secure_cookies)
                    .await;
            }
            return Err(AuthError(StatusCode::FORBIDDEN, None));
        }
        Err(error) => return Err(error),
    };
    sqlx::query("UPDATE users SET display_name=$2 WHERE id=$1 AND cleaned_at IS NULL AND display_name IS DISTINCT FROM $2")
        .bind(user_id)
        .bind(display_name)
        .execute(&state.store.pool)
        .await
        .map_err(internal)?;
    // Upstream access/refresh tokens are never persisted or forwarded.
    drop(tokens);
    let session = random_token()?;
    let csrf = random_token()?;
    // Device summary for "Your sessions": a fixed-vocabulary browser/OS family only.
    let label = client_label::client_label(
        headers
            .get(header::USER_AGENT)
            .and_then(|v| v.to_str().ok()),
    );
    // Both deadlines are fixed now, on the database clock (#80).
    // Sign-in times live on the user, in the same statement, so deleting
    // ended sessions (#80) never changes "First signed in" or "Last sign-in".
    let inserted: i64 = sqlx::query_scalar("WITH s AS (INSERT INTO browser_sessions(token_hash,user_id,csrf_hash,expires_at,idle_expires_at,verified_email,client_label,last_seen_at) SELECT $1,id,$3,now()+make_interval(secs=>$6),now()+make_interval(secs=>$7::float8),$4,$5,now() FROM users WHERE id=$2 AND disabled_at IS NULL AND cleaned_at IS NULL AND EXISTS(SELECT 1 FROM effective_platform_roles p WHERE p.user_id=users.id) RETURNING user_id), u AS (UPDATE users SET last_signed_in_at=now(),first_signed_in_at=coalesce(first_signed_in_at,now()) WHERE id IN (SELECT user_id FROM s)) SELECT count(*) FROM s")
        .bind(hash(&session)).bind(user_id).bind(hash(&csrf)).bind(&email).bind(label)
        .bind(f64::from(state.policy.lifetime_seconds())).bind(state.policy.idle_seconds())
        .fetch_one(&state.store.pool).await.map_err(internal)?;
    if inserted != 1 {
        if wants_html(&headers) {
            return denied_browser_callback(&state, &headers, provider.config.secure_cookies).await;
        }
        return Err(AuthError(StatusCode::UNAUTHORIZED, None));
    }
    // Validated again on the way out; anything else lands on "/" (Home).
    let target = return_to
        .as_deref()
        .and_then(safe_return_path)
        .unwrap_or_else(|| "/".into());
    let mut response = private(Redirect::to(&target).into_response());
    set_cookie(
        &mut response,
        SESSION,
        &session,
        state.policy.lifetime_seconds(),
        provider.config.secure_cookies,
        true,
    );
    set_cookie(
        &mut response,
        CSRF,
        &csrf,
        state.policy.lifetime_seconds(),
        provider.config.secure_cookies,
        false,
    );
    set_cookie(
        &mut response,
        BROWSER,
        "",
        0,
        provider.config.secure_cookies,
        true,
    );
    Ok(response)
}

async fn advisory_lock(tx: &mut Transaction<'_, Postgres>, key: &str) -> Result<(), AuthError> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,0))")
        .bind(key)
        .execute(&mut **tx)
        .await
        .map_err(internal)?;
    Ok(())
}

#[cfg(all(test, feature = "integration-tests"))]
pub(super) async fn resolve_identity(
    store: &Store,
    issuer: &str,
    subject: &str,
    email: &str,
    groups: &[String],
) -> Result<Uuid, AuthError> {
    resolve_identity_with(store, issuer, subject, email, groups, false).await
}

/// With SCIM enabled, SCIM-managed groups take their membership from SCIM, not the
/// token: the claim is merged as (token groups not managed by SCIM) ∪ (SCIM groups).
#[cfg_attr(
    not(test),
    expect(clippy::too_many_lines, clippy::cognitive_complexity, reason = "#499")
)]
pub(super) async fn resolve_identity_with(
    store: &Store,
    issuer: &str,
    subject: &str,
    email: &str,
    groups: &[String],
    scim: bool,
) -> Result<Uuid, AuthError> {
    let email = email.to_lowercase();
    let mut tx = crate::db::begin(&store.pool).await.map_err(internal)?;
    lifecycle::lock(&mut tx).await.map_err(internal)?;
    advisory_lock(&mut tx, &format!("oidc:{}:{issuer}{subject}", issuer.len())).await?;
    // Every account this sign-in may change (the linked one, which may be
    // cleaned up, and the one its email resolves to): authority scope locks
    // up front, in canonical order (`lifecycle::lock_users`). New accounts
    // (the id an account created here gets) have no keys or workspaces yet.
    let new_user = Uuid::new_v4();
    let mut candidates: Vec<Uuid> = sqlx::query_scalar("SELECT user_id FROM oidc_identities WHERE issuer=$1 AND subject=$2 UNION SELECT id FROM users WHERE lower(email)=$3")
        .bind(issuer).bind(subject).bind(&email).fetch_all(&mut *tx).await.map_err(internal)?;
    candidates.push(new_user);
    lifecycle::lock_users(&mut tx, &candidates)
        .await
        .map_err(internal)?;
    let existing = sqlx::query_as::<_, (Uuid, bool, bool, Option<String>)>(
        "SELECT u.id,u.cleaned_at IS NOT NULL,coalesce(u.cleanup_due_at<=now(),false),u.disable_reason FROM oidc_identities i JOIN users u ON u.id=i.user_id WHERE i.issuer=$1 AND i.subject=$2 FOR UPDATE OF u",
    ).bind(issuer).bind(subject).fetch_optional(&mut *tx).await.map_err(internal)?;
    let mut rebind = false;
    let existing_id = if let Some((id, cleaned, expired, reason)) = existing {
        if cleaned || expired {
            // On legal hold (#75): nothing is removed and the account stays
            // closed until the hold is released.
            if !cleaned
                && !lifecycle::cleanup_user(&mut tx, id)
                    .await
                    .map_err(internal)?
            {
                tx.rollback().await.map_err(internal)?;
                return Err(AuthError(StatusCode::FORBIDDEN, None));
            }
            if reason.as_deref() != Some("entitlement_loss") {
                tx.commit().await.map_err(internal)?;
                return Err(AuthError(StatusCode::FORBIDDEN, None));
            }
            let reentitled: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM oidc_group_mappings WHERE issuer=$1 AND enabled AND target_kind='platform' AND group_value=ANY($2))")
                .bind(issuer).bind(groups).fetch_one(&mut *tx).await.map_err(internal)?;
            if !reentitled {
                tx.commit().await.map_err(internal)?;
                return Err(AuthError(StatusCode::FORBIDDEN, None));
            }
            rebind = true;
            None
        } else {
            Some(id)
        }
    } else {
        None
    };
    let id = if let Some(id) = existing_id {
        id
    } else {
        advisory_lock(&mut tx, &format!("email:{email}")).await?;
        let created = sqlx::query_scalar::<_, Uuid>(
            "INSERT INTO users(id,email) VALUES($1,$2) ON CONFLICT DO NOTHING RETURNING id",
        )
        .bind(new_user)
        .bind(&email)
        .fetch_optional(&mut *tx)
        .await
        .map_err(internal)?;
        let id = if let Some(id) = created {
            id
        } else {
            let (id, active, allowed) = sqlx::query_as::<_, (Uuid, bool, bool)>("SELECT id,disabled_at IS NULL AND cleaned_at IS NULL,oidc_link_allowed FROM users WHERE lower(email)=$1 FOR UPDATE")
                .bind(&email).fetch_one(&mut *tx).await.map_err(internal)?;
            if !active {
                return Err(AuthError(StatusCode::FORBIDDEN, None));
            }
            let linked: bool =
                sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM oidc_identities WHERE user_id=$1)")
                    .bind(id)
                    .fetch_one(&mut *tx)
                    .await
                    .map_err(internal)?;
            if !allowed || linked {
                return Err(AuthError(StatusCode::CONFLICT, None));
            }
            sqlx::query("UPDATE users SET oidc_link_allowed=false WHERE id=$1")
                .bind(id)
                .execute(&mut *tx)
                .await
                .map_err(internal)?;
            id
        };
        if rebind {
            sqlx::query("UPDATE oidc_identities SET user_id=$3 WHERE issuer=$1 AND subject=$2")
                .bind(issuer)
                .bind(subject)
                .bind(id)
                .execute(&mut *tx)
                .await
                .map_err(internal)?;
            sqlx::query("INSERT INTO audit_events(id,actor_user_id,action,resource_type,resource_id) VALUES($1,$2,'identity.rebound','user',$2)")
                .bind(Uuid::new_v4()).bind(id).execute(&mut *tx).await.map_err(internal)?;
        } else {
            sqlx::query("INSERT INTO oidc_identities(issuer,subject,user_id) VALUES($1,$2,$3)")
                .bind(issuer)
                .bind(subject)
                .bind(id)
                .execute(&mut *tx)
                .await
                .map_err(internal)?;
        }
        id
    };
    let groups = if scim {
        crate::scim::effective_groups(&mut tx, id, groups)
            .await
            .map_err(internal)?
    } else {
        groups.to_vec()
    };
    let active = lifecycle::synchronize_groups(&mut tx, id, issuer, &groups)
        .await
        .map_err(internal)?;
    // Entitlement loss MUST commit session/key revocation before returning the 403.
    tx.commit().await.map_err(internal)?;
    if !active {
        return Err(AuthError(StatusCode::FORBIDDEN, None));
    }
    Ok(id)
}

fn wants_html(headers: &HeaderMap) -> bool {
    headers
        .get(header::ACCEPT)
        .and_then(|h| h.to_str().ok())
        .is_some_and(|h| {
            h.split(',')
                .any(|part| part.trim().split(';').next() == Some("text/html"))
        })
}

// Only called after nonce-bound signature verification and a live access denial.
// Do not turn malformed/unverified callbacks into a cross-site logout primitive.
async fn denied_browser_callback(
    state: &IdentityState,
    headers: &HeaderMap,
    secure: bool,
) -> Result<Response, AuthError> {
    if let Some(token) = cookie(headers, SESSION)? {
        sqlx::query(
            "UPDATE browser_sessions SET revoked_at=coalesce(revoked_at,now()) WHERE token_hash=$1",
        )
        .bind(hash(&token))
        .execute(&state.store.pool)
        .await
        .map_err(internal)?;
    }
    let mut response = private(Redirect::to("/?auth_error=access_denied").into_response());
    set_cookie(&mut response, SESSION, "", 0, secure, true);
    set_cookie(&mut response, CSRF, "", 0, secure, false);
    set_cookie(&mut response, BROWSER, "", 0, secure, true);
    Ok(response)
}

pub(super) async fn logout(
    State(state): State<IdentityState>,
    headers: HeaderMap,
) -> Result<Response, AuthError> {
    let session = verify_session(&state, &headers).await?;
    verify_csrf(&state, &headers, &session.csrf_hash)?;
    sqlx::query(
        "UPDATE browser_sessions SET revoked_at=now() WHERE token_hash=$1 AND revoked_at IS NULL",
    )
    .bind(session.token_hash)
    .execute(&state.store.pool)
    .await
    .map_err(internal)?;
    let secure = state
        .provider
        .as_ref()
        .is_none_or(|provider| provider.config.secure_cookies);
    let mut response = private(StatusCode::NO_CONTENT.into_response());
    set_cookie(&mut response, SESSION, "", 0, secure, true);
    set_cookie(&mut response, CSRF, "", 0, secure, false);
    set_cookie(&mut response, BROWSER, "", 0, secure, true);
    Ok(response)
}
