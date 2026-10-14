-- #80: an idle timeout per session, and sign-in times on the user so that
-- deleting ended sessions never changes what admins see (backfilled in 0043).
--
-- Existing sessions keep their absolute expiry and get an idle deadline from
-- their next recorded activity (NULL until then: no idle limit).
--
-- Nullable columns without defaults: metadata only, so the ACCESS EXCLUSIVE
-- locks are brief. Inference admission reads `users`, so don't wait long for
-- them: fail and retry the migration instead of queueing admissions.
SET lock_timeout = '10s';
ALTER TABLE browser_sessions ADD COLUMN idle_expires_at timestamptz;
ALTER TABLE users
    ADD COLUMN first_signed_in_at timestamptz,
    ADD COLUMN last_signed_in_at timestamptz;
RESET lock_timeout;
