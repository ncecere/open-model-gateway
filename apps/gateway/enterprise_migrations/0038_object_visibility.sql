-- Object visibility (#141): who in a shared (Team/Project) workspace sees its
-- files, batches (with their result files) and video jobs.
--   member    (default): a key issued to a regular member sees only objects
--             created by keys issued to that same user (files: the uploader,
--             `stored_files.created_by_user_id`); a regular member in the
--             dashboard sees their own. Service-account keys and workspace
--             owners/admins see every object.
--   workspace: every member's key, and every member in the dashboard, sees
--             all of them.
-- Personal workspaces stay owner-private; the value is ignored for them.
-- Existing shared workspaces become `member` (a breaking change, see
-- docs/releases/unreleased.md).
--
-- Metadata-only (constant default, no table rewrite). Visibility is not an
-- admission input: no authority trigger (0027) or change-notification
-- trigger (0028) watches this column, so changing it takes no scope lock.
ALTER TABLE workspaces ADD COLUMN object_visibility text NOT NULL DEFAULT 'member'
 CHECK (object_visibility IN ('member','workspace'));
