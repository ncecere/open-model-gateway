-- #78: sign-in counts unexpired attempts against an installation-wide cap,
-- and #80 cleanup deletes expired ones in batches. Both read by expiry.
CREATE INDEX oidc_login_attempts_expires ON oidc_login_attempts(expires_at);
