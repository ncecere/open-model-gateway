-- #83 slice B: a member of a shared workspace requests selection of a
-- catalog-eligible model for it; an owner/admin of that workspace approves.
-- Additive: existing (membership) requests are unchanged.
ALTER TABLE access_requests DROP CONSTRAINT membership_requests_only;
ALTER TABLE access_requests ADD COLUMN model_id uuid REFERENCES models(id);
-- Frozen submitted reference, like workspace_name_at_request.
ALTER TABLE access_requests ADD COLUMN model_name_at_request text NOT NULL DEFAULT '';
ALTER TABLE access_requests ALTER COLUMN requested_role DROP NOT NULL;
ALTER TABLE access_requests ALTER COLUMN requested_role DROP DEFAULT;
ALTER TABLE access_requests DROP CONSTRAINT access_requests_requested_role_check;
ALTER TABLE access_requests ADD CONSTRAINT access_request_target CHECK (
 (target_kind='workspace_membership' AND model_id IS NULL AND requested_role='member')
 OR (target_kind='model_selection' AND model_id IS NOT NULL AND requested_role IS NULL));
-- One pending request per requester and target.
DROP INDEX access_requests_pending_target;
CREATE UNIQUE INDEX access_requests_pending_membership ON access_requests(requester,workspace_id) WHERE status='pending' AND target_kind='workspace_membership';
CREATE UNIQUE INDEX access_requests_pending_model ON access_requests(requester,workspace_id,model_id) WHERE status='pending' AND target_kind='model_selection';

-- An application names either the membership grant or the selected model.
-- The model selection may later be removed; the record stays.
ALTER TABLE access_request_applications ALTER COLUMN membership_grant_id DROP NOT NULL;
ALTER TABLE access_request_applications ADD COLUMN model_id uuid REFERENCES models(id);
ALTER TABLE access_request_applications ADD CONSTRAINT access_request_application_target CHECK (num_nonnulls(membership_grant_id,model_id)=1);

CREATE OR REPLACE FUNCTION access_requests_guard() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE t timestamptz; wait_seconds integer;
BEGIN
 IF TG_OP='INSERT' THEN
  -- A private per-requester lock serializes the rolling-window counts across replicas.
  PERFORM pg_advisory_xact_lock(hashtextextended('access-request:'||NEW.requester::text,0));
  t:=clock_timestamp();
  IF NEW.status<>'pending' OR NEW.decided_by IS NOT NULL OR NEW.decided_at IS NOT NULL THEN
   RAISE EXCEPTION 'pending initial state required' USING ERRCODE='23514';
  END IF;
  NEW.created_at:=t; NEW.expires_at:=t+interval '7 days';
  -- The runtime cannot insert the kind, role or frozen names: they derive
  -- from the target here, always overwriting a supplied value.
  IF NEW.model_id IS NULL THEN
   NEW.target_kind:='workspace_membership'; NEW.requested_role:='member'; NEW.model_name_at_request:='';
  ELSE
   NEW.target_kind:='model_selection'; NEW.requested_role:=NULL;
   NEW.model_name_at_request:=coalesce((SELECT m.public_name FROM models m WHERE m.id=NEW.model_id),'');
  END IF;
  NEW.workspace_name_at_request:=coalesce((SELECT w.name FROM workspaces w WHERE w.id=NEW.workspace_id),'');
  IF NEW.target_kind='workspace_membership' AND NOT EXISTS(SELECT FROM workspaces WHERE id=NEW.workspace_id AND kind IN ('team','project') AND discoverable AND disabled_at IS NULL) THEN
   RAISE EXCEPTION 'ineligible target' USING ERRCODE='23514';
  END IF;
  -- Backstop for the handler's checks: a current member of an enabled shared
  -- workspace, for an enabled model. Catalog eligibility is the handler's
  -- check (and approval's recheck); a request selects nothing by itself.
  IF NEW.target_kind='model_selection' AND (
     NOT EXISTS(SELECT FROM workspaces WHERE id=NEW.workspace_id AND kind IN ('team','project') AND disabled_at IS NULL)
     OR NOT EXISTS(SELECT FROM effective_workspace_memberships e WHERE e.workspace_id=NEW.workspace_id AND e.user_id=NEW.requester)
     OR NOT EXISTS(SELECT FROM models WHERE id=NEW.model_id AND enabled)) THEN
   RAISE EXCEPTION 'ineligible target' USING ERRCODE='23514';
  END IF;
  -- A queue bound for model requests: at most 5 pending per requester and
  -- workspace, and 100 pending per workspace (a membership request is one
  -- per requester and workspace by its unique index). Not a rate limit: a
  -- slot frees when an owner/admin decides or the request expires, so it has
  -- its own code. The per-requester lock above does not serialize different
  -- requesters; the workspace count relies on the handler holding the
  -- workspace's exclusive authority lock (scope_locks) for every insert.
  IF NEW.target_kind='model_selection' AND (
     (SELECT count(*) FROM access_requests WHERE requester=NEW.requester AND workspace_id=NEW.workspace_id AND target_kind='model_selection' AND status='pending' AND expires_at>t)>=5
     OR (SELECT count(*) FROM access_requests WHERE workspace_id=NEW.workspace_id AND target_kind='model_selection' AND status='pending' AND expires_at>t)>=100) THEN
   RAISE EXCEPTION 'model request queue full' USING ERRCODE='P0084';
  END IF;
  IF (SELECT count(*) FROM access_requests WHERE requester=NEW.requester AND status='pending' AND expires_at>t)>=20 THEN
   RAISE EXCEPTION 'request cap' USING ERRCODE='P0083',DETAIL='60';
  END IF;
  IF (SELECT count(*) FROM access_requests WHERE requester=NEW.requester AND created_at>t-interval '1 minute')>=5 THEN
   SELECT greatest(1,ceil(extract(epoch FROM min(created_at)+interval '1 minute'-t))::integer) INTO wait_seconds FROM access_requests WHERE requester=NEW.requester AND created_at>t-interval '1 minute';
   RAISE EXCEPTION 'request cap' USING ERRCODE='P0083',DETAIL=wait_seconds::text;
  END IF;
  IF (SELECT count(*) FROM access_requests WHERE requester=NEW.requester AND created_at>t-interval '1 day')>=50 THEN
   SELECT greatest(1,ceil(extract(epoch FROM min(created_at)+interval '1 day'-t))::integer) INTO wait_seconds FROM access_requests WHERE requester=NEW.requester AND created_at>t-interval '1 day';
   RAISE EXCEPTION 'request cap' USING ERRCODE='P0083',DETAIL=wait_seconds::text;
  END IF;
  RETURN NEW;
 END IF;
 -- Only the owner-run cleanup trigger/bounded scrubber can scrub the reason. Runtime has no
 -- UPDATE(reason); legal holds are evaluated again by this backstop.
 IF NEW.reason IS DISTINCT FROM OLD.reason AND NEW.reason IS NULL
    AND (to_jsonb(NEW)-'reason')=(to_jsonb(OLD)-'reason')
    AND EXISTS(SELECT FROM users WHERE id=OLD.requester AND cleaned_at IS NOT NULL)
    AND NOT EXISTS(SELECT FROM legal_holds WHERE released_at IS NULL AND (user_id=OLD.requester OR workspace_id=OLD.workspace_id)) THEN
  RETURN NEW;
 END IF;
 IF (to_jsonb(NEW)-ARRAY['status','decided_by','decided_at']) IS DISTINCT FROM (to_jsonb(OLD)-ARRAY['status','decided_by','decided_at']) OR OLD.status<>'pending' OR NEW.status='pending' THEN
  RAISE EXCEPTION 'immutable request or terminal state' USING ERRCODE='23514';
 END IF;
 t:=clock_timestamp();
 IF (NEW.status='expired')<>(OLD.expires_at<=t) THEN
  RAISE EXCEPTION 'request expiry required' USING ERRCODE='23514';
 END IF;
 NEW.decided_at:=t;
 RETURN NEW;
END $$;

-- Approved means applied. A membership application names a manual member
-- grant in the request's scope (as in 0066); a model application names the
-- request's model, and at application the workspace has it selected from
-- its catalog. Deferred so both sides commit atomically.
CREATE OR REPLACE FUNCTION access_request_application_check() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE rid uuid;
BEGIN
 IF TG_TABLE_NAME='access_requests' THEN rid:=NEW.id; ELSE rid:=NEW.request_id; END IF;
 IF EXISTS(SELECT FROM access_requests r WHERE r.id=rid AND
   (((r.status='approved')<>EXISTS(SELECT FROM access_request_applications a WHERE a.request_id=r.id))
    OR EXISTS(SELECT FROM access_request_applications a WHERE a.request_id=r.id AND (
      (r.target_kind='workspace_membership' AND (a.model_id IS NOT NULL OR NOT EXISTS(
        SELECT FROM workspace_membership_grants g WHERE g.id=a.membership_grant_id AND g.workspace_id=a.workspace_id
        AND g.user_id=a.requester AND g.source='manual' AND g.role='member')))
      OR (r.target_kind='model_selection' AND (a.membership_grant_id IS NOT NULL OR a.model_id IS DISTINCT FROM r.model_id))
    )))) THEN
  RAISE EXCEPTION 'approval requires matching application' USING ERRCODE='23514';
 END IF;
 IF TG_TABLE_NAME='access_request_applications' AND NEW.model_id IS NOT NULL AND NOT EXISTS(
   SELECT FROM workspace_model_grants g WHERE g.workspace_id=NEW.workspace_id AND g.model_id=NEW.model_id AND g.source='catalog') THEN
  RAISE EXCEPTION 'model application requires the catalog selection' USING ERRCODE='23514';
 END IF;
 RETURN NULL;
END $$;
