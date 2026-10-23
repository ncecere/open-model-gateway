-- #37 PR2: authentication, API and audit for admin API tokens.
--
-- Two additions:
--
-- 1. `audit_events.admin_token_id`: business-action audit rows (the
--    existing `audit_events` table, written transactionally alongside the
--    mutation it describes) now carry which admin token, if any, performed
--    the action -- "Business-action audit events carry token attribution"
--    (coordinator decision). NULL for every browser-session action, exactly
--    as before. References `admin_api_tokens(id)`, which is never deleted
--    (0051: no DELETE, no TRUNCATE), so this FK is always resolvable. The
--    partial index below is the "audit token-attribution index" the
--    approved spec's schema section describes; omitted from 0051/0050
--    because the column it attributes did not exist until now.
--
-- 2. `admin_token_attempts`: a *separate*, append-only record of every
--    admitted admin-token request attempt -- distinct from `audit_events`,
--    which records business actions (what changed), not access attempts
--    (who tried to reach what, and whether they were let through). Every
--    row is one immutable event (a `start` row persisted before dispatch, a
--    `completion` row appended after the response is ready), linked by
--    `request_id` (the same server-generated id already in every
--    response's `x-request-id` header and the `http.request` tracing span;
--    each HTTP request has exactly one, so it is already the correlator,
--    with no need for a second generated id): this (not an UPDATE of a
--    single row) is what "persist an immutable attempt-start... and append
--    completion" means here, and gives the crash-safety property for free
--    -- a crash between the two writes leaves exactly a `start` row with
--    no matching `completion`, which *is* the honest record of an
--    incomplete attempt, rather than a half-written row. Never raw URLs,
--    bodies, names, credentials or hashes: only the token id, its owner,
--    the server request id, the route's exact method/template, the scope
--    the route required (NULL for a Denied/BrowserAdminOnly route) and the
--    admission outcome/status.
--
-- Pre-authentication and over-limit floods (a credential that never
-- authenticates, or a per-replica rate limit exceeded before a token
-- principal exists at all) are never recorded here (coordinator decision):
-- there is no token id to attribute them to, and admitting unauthenticated
-- noise into an audit table would itself be a flood vector. Those are
-- bounded per-replica rate limiting with aggregated metric counting only
-- (apps/gateway/src/identity.rs).
ALTER TABLE audit_events ADD COLUMN admin_token_id uuid REFERENCES admin_api_tokens(id);
CREATE INDEX audit_events_token_attribution ON audit_events(admin_token_id,created_at) WHERE admin_token_id IS NOT NULL;

CREATE TABLE admin_token_attempts(
 id uuid PRIMARY KEY,
 request_id uuid NOT NULL,
 phase text NOT NULL CHECK(phase IN ('start','completion')),
 token_id uuid NOT NULL REFERENCES admin_api_tokens(id),
 owner_user_id uuid NOT NULL REFERENCES users(id),
 route_method text NOT NULL CHECK(char_length(route_method) BETWEEN 1 AND 16),
 route_template text NOT NULL CHECK(char_length(route_template) BETWEEN 1 AND 256),
 required_scope text CHECK(required_scope IS NULL OR required_scope=ANY(ARRAY['platform:read','catalog:write','access:write','governance:write','settings:write']::text[])),
 -- The admission decision: made (and immutable) at the `start` row,
 -- repeated verbatim on the matching `completion` row so a reader never
 -- needs to join the pair to know whether the attempt was let through.
 outcome text NOT NULL CHECK(outcome IN ('allowed','denied')),
 -- The response status actually returned. NULL on every `start` row
 -- (nothing has returned yet); required on every `completion` row.
 status_code smallint CHECK(status_code IS NULL OR status_code BETWEEN 100 AND 599),
 CHECK((phase='start' AND status_code IS NULL) OR (phase='completion' AND status_code IS NOT NULL)),
 created_at timestamptz NOT NULL DEFAULT clock_timestamp()
);
CREATE INDEX admin_token_attempts_request ON admin_token_attempts(request_id,phase);
CREATE INDEX admin_token_attempts_token_time ON admin_token_attempts(token_id,created_at DESC);
-- Append-only, like every other audit table in this schema (0001's
-- immutable_history(), reused verbatim): no UPDATE or DELETE, ever, not
-- even by a schema owner connection, and no TRUNCATE.
CREATE TRIGGER admin_token_attempts_no_update BEFORE UPDATE ON admin_token_attempts
 FOR EACH ROW EXECUTE FUNCTION immutable_history();
CREATE TRIGGER admin_token_attempts_no_delete BEFORE DELETE ON admin_token_attempts
 FOR EACH ROW EXECUTE FUNCTION immutable_history();
CREATE TRIGGER admin_token_attempts_no_truncate BEFORE TRUNCATE ON admin_token_attempts
 FOR EACH STATEMENT EXECUTE FUNCTION immutable_history();
