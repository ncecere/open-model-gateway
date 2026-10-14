-- #79: users list and revoke their own dashboard sessions.
-- `id` is the public handle (never the token hash); `last_seen_at` is touched
-- at most every 5 minutes; `client_label` is a browser/OS family from a fixed
-- vocabulary (no IP address, no raw user agent).
ALTER TABLE browser_sessions
    ADD COLUMN id uuid NOT NULL DEFAULT gen_random_uuid(),
    ADD COLUMN last_seen_at timestamptz,
    ADD COLUMN client_label text CHECK (client_label ~ '^[A-Za-z0-9 ]{1,40}$');
CREATE UNIQUE INDEX browser_sessions_public_id ON browser_sessions(id);
