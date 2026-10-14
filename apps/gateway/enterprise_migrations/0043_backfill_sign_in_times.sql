-- #80: sign-in times from existing sessions, before cleanup can delete any.
-- Display only: never used for access or inactivity decisions. Row locks on
-- users (not a table lock) until this commits; admission of an affected
-- user's key waits for that moment.
UPDATE users u SET first_signed_in_at=s.first, last_signed_in_at=s.last
FROM (SELECT user_id, min(created_at) AS first, max(created_at) AS last
      FROM browser_sessions GROUP BY user_id) s
WHERE s.user_id=u.id AND u.first_signed_in_at IS NULL;
