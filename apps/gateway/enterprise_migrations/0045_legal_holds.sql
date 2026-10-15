-- #75: legal holds. A Platform Admin places a hold on a user or on a team
-- or project workspace; while it is active, the gateway doesn't remove that
-- scope's data: departed-user cleanup, request-log compaction, stored-file
-- expiry and `archive partition` all skip or refuse it. A hold grants no read
-- access. The reference is a short code (a ticket id), never free text.
-- A personal workspace is held through its owner's user hold.

CREATE TABLE legal_holds(
 id uuid PRIMARY KEY,
 user_id uuid REFERENCES users(id),
 workspace_id uuid REFERENCES workspaces(id),
 reference text NOT NULL CHECK(reference ~ '^[A-Za-z0-9][A-Za-z0-9._:/#-]{0,63}$'),
 created_by_user_id uuid REFERENCES users(id),
 created_at timestamptz NOT NULL DEFAULT now(),
 released_at timestamptz,
 released_by_user_id uuid REFERENCES users(id),
 CHECK(num_nonnulls(user_id, workspace_id) = 1),
 CHECK(released_at IS NOT NULL OR released_by_user_id IS NULL)
);
CREATE INDEX legal_holds_active_user ON legal_holds(user_id) WHERE released_at IS NULL AND user_id IS NOT NULL;
CREATE INDEX legal_holds_active_workspace ON legal_holds(workspace_id) WHERE released_at IS NULL AND workspace_id IS NOT NULL;
-- At most one active hold per target and reference.
CREATE UNIQUE INDEX legal_holds_active_reference ON legal_holds(coalesce(user_id, workspace_id), lower(reference)) WHERE released_at IS NULL;

CREATE FUNCTION omg_legal_holds_guard() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 IF TG_OP IN ('DELETE','TRUNCATE') THEN
  RAISE EXCEPTION 'legal holds are released, never removed' USING ERRCODE='23514';
 END IF;
 IF TG_OP = 'INSERT' THEN
  IF NEW.workspace_id IS NOT NULL AND NOT EXISTS(
       SELECT 1 FROM workspaces w WHERE w.id=NEW.workspace_id AND w.kind IN ('team','project')) THEN
   RAISE EXCEPTION 'hold a personal workspace through its owner' USING ERRCODE='23514';
  END IF;
  RETURN NEW;
 END IF;
 -- UPDATE: only releasing, once, by a named user, at the database's time;
 -- nothing else changes, and a released hold is never touched again.
 IF OLD.released_at IS NOT NULL OR NEW.released_at IS NULL OR NEW.released_by_user_id IS NULL
    OR NEW.id<>OLD.id OR NEW.user_id IS DISTINCT FROM OLD.user_id OR NEW.workspace_id IS DISTINCT FROM OLD.workspace_id
    OR NEW.reference<>OLD.reference OR NEW.created_at<>OLD.created_at
    OR NEW.created_by_user_id IS DISTINCT FROM OLD.created_by_user_id THEN
  RAISE EXCEPTION 'a legal hold can only be released, once, by a named user' USING ERRCODE='23514';
 END IF;
 NEW.released_at := now();
 RETURN NEW;
END $$;
CREATE TRIGGER legal_holds_guard BEFORE INSERT OR UPDATE OR DELETE ON legal_holds
 FOR EACH ROW EXECUTE FUNCTION omg_legal_holds_guard();
CREATE TRIGGER legal_holds_no_truncate BEFORE TRUNCATE ON legal_holds
 FOR EACH STATEMENT EXECUTE FUNCTION omg_legal_holds_guard();
