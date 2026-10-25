-- #83 slice A. Explicit fresh-database migration; 0059–0065 reserved by open PRs.
ALTER TABLE workspaces ADD COLUMN discoverable boolean NOT NULL DEFAULT false;
ALTER TABLE workspaces ADD CONSTRAINT discoverable_shared CHECK (NOT discoverable OR kind IN ('team','project'));
-- Approval's workspace lock also excludes an out-of-handler opt-out.
CREATE TRIGGER access_request_discoverable_lock BEFORE UPDATE OF discoverable ON workspaces FOR EACH ROW EXECUTE FUNCTION omg_authority_lock('workspace','id');

CREATE TABLE access_requests (
 id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
 requester uuid NOT NULL REFERENCES users(id),
 target_kind text NOT NULL DEFAULT 'workspace_membership' CHECK (target_kind IN ('workspace_membership','model_selection')),
 workspace_id uuid NOT NULL REFERENCES workspaces(id),
 requested_role text NOT NULL DEFAULT 'member' CHECK (requested_role='member'),
 -- model_selection is reserved, not an accepted target in slice A.
 CONSTRAINT membership_requests_only CHECK (target_kind='workspace_membership'),
 status text NOT NULL DEFAULT 'pending' CHECK (status IN ('pending','approved','denied','withdrawn','expired')),
 reason text CHECK (char_length(reason)<=1000 AND reason !~ '[[:cntrl:]]'),
 idempotency_digest bytea NOT NULL CHECK (octet_length(idempotency_digest)=32),
 payload_digest bytea NOT NULL CHECK (octet_length(payload_digest)=32),
 created_at timestamptz NOT NULL DEFAULT clock_timestamp(),
 expires_at timestamptz NOT NULL DEFAULT (clock_timestamp()+interval '7 days'),
 decided_by uuid REFERENCES users(id),
 decided_at timestamptz,
 UNIQUE(requester,idempotency_digest),
 UNIQUE(workspace_id,requester,id),
 CHECK ((status='pending')=(decided_at IS NULL)),
 CHECK (status NOT IN ('approved','denied') OR (decided_by IS NOT NULL AND decided_by<>requester)),
 CHECK (status<>'withdrawn' OR (decided_by IS NOT NULL AND decided_by=requester)),
 CHECK (status<>'expired' OR decided_by IS NULL)
);
CREATE UNIQUE INDEX access_requests_pending_target ON access_requests(requester,target_kind,workspace_id) WHERE status='pending';
CREATE INDEX access_requests_requester_clock ON access_requests(requester,created_at DESC,id);
CREATE INDEX access_requests_platform_clock ON access_requests(created_at DESC,id);
CREATE INDEX access_requests_reason_cleanup ON access_requests(requester,created_at,id) WHERE reason IS NOT NULL;
CREATE INDEX access_requests_workspace_clock ON access_requests(workspace_id,created_at DESC,id);
CREATE INDEX access_requests_expiry ON access_requests(expires_at,id) WHERE status='pending';
ALTER TABLE workspace_membership_grants ADD CONSTRAINT membership_request_scope UNIQUE(workspace_id,user_id,id);
-- Existing grant/revocation paths use transaction-time now(). A transaction
-- begun before submission may commit a later membership change, so those
-- timestamps cannot safely detect an intervening authorization decision.
ALTER TABLE workspace_membership_grants ADD COLUMN access_request_changed_at timestamptz NOT NULL DEFAULT clock_timestamp();
CREATE INDEX membership_access_request_changes ON workspace_membership_grants(workspace_id,user_id,access_request_changed_at);
CREATE FUNCTION access_request_membership_clock() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 -- Runtime INSERT is table-level: always overwrite a supplied clock. The
 -- runtime has no UPDATE privilege on this column. Revocations set it even
 -- when their caller's transaction began before the request's creation.
 IF TG_OP='INSERT' OR NEW.revoked_at IS DISTINCT FROM OLD.revoked_at THEN
  NEW.access_request_changed_at:=clock_timestamp();
 END IF;
 RETURN NEW;
END $$;
CREATE TRIGGER access_request_membership_clock BEFORE INSERT OR UPDATE OF revoked_at ON workspace_membership_grants FOR EACH ROW EXECUTE FUNCTION access_request_membership_clock();
CREATE TABLE access_request_applications (
 request_id uuid PRIMARY KEY,
 workspace_id uuid NOT NULL,
 requester uuid NOT NULL,
 membership_grant_id uuid NOT NULL UNIQUE,
 created_at timestamptz NOT NULL DEFAULT clock_timestamp(),
 FOREIGN KEY(workspace_id,requester,request_id) REFERENCES access_requests(workspace_id,requester,id),
 FOREIGN KEY(workspace_id,requester,membership_grant_id) REFERENCES workspace_membership_grants(workspace_id,user_id,id)
);

CREATE FUNCTION access_requests_guard() RETURNS trigger LANGUAGE plpgsql AS $$
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
  IF NOT EXISTS(SELECT FROM workspaces WHERE id=NEW.workspace_id AND kind IN ('team','project') AND discoverable AND disabled_at IS NULL) THEN
   RAISE EXCEPTION 'ineligible target' USING ERRCODE='23514';
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
CREATE TRIGGER access_requests_guard BEFORE INSERT OR UPDATE ON access_requests FOR EACH ROW EXECUTE FUNCTION access_requests_guard();
CREATE TRIGGER access_requests_no_delete BEFORE DELETE ON access_requests FOR EACH ROW EXECUTE FUNCTION immutable_history();
CREATE TRIGGER access_requests_no_truncate BEFORE TRUNCATE ON access_requests FOR EACH STATEMENT EXECUTE FUNCTION immutable_history();
CREATE TRIGGER access_request_applications_immutable BEFORE UPDATE OR DELETE ON access_request_applications FOR EACH ROW EXECUTE FUNCTION immutable_history();
CREATE TRIGGER access_request_applications_no_truncate BEFORE TRUNCATE ON access_request_applications FOR EACH STATEMENT EXECUTE FUNCTION immutable_history();

-- Approved means applied, and linkage can only name a manual member grant in
-- precisely the request's scope. Deferred so both sides commit atomically.
CREATE FUNCTION access_request_application_check() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE rid uuid;
BEGIN
 IF TG_TABLE_NAME='access_requests' THEN rid:=NEW.id; ELSE rid:=NEW.request_id; END IF;
 IF EXISTS(SELECT FROM access_requests r WHERE r.id=rid AND
   (((r.status='approved')<>EXISTS(SELECT FROM access_request_applications a WHERE a.request_id=r.id))
    OR EXISTS(SELECT FROM access_request_applications a JOIN workspace_membership_grants g ON g.id=a.membership_grant_id AND g.workspace_id=a.workspace_id AND g.user_id=a.requester WHERE a.request_id=r.id AND (g.source<>'manual' OR g.role<>'member')))) THEN
  RAISE EXCEPTION 'approval requires matching manual membership application' USING ERRCODE='23514';
 END IF;
 RETURN NULL;
END $$;
CREATE CONSTRAINT TRIGGER access_requests_application AFTER INSERT OR UPDATE ON access_requests DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION access_request_application_check();
CREATE CONSTRAINT TRIGGER access_request_applications_check AFTER INSERT ON access_request_applications DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION access_request_application_check();

-- Narrow owner-run personal-data erasure, not an UPDATE(reason) runtime ACL.
-- Called after cleaned_at is written inside existing hold-aware cleanup.
CREATE FUNCTION access_request_reason_cleanup() RETURNS trigger LANGUAGE plpgsql SECURITY DEFINER SET search_path=public,pg_temp AS $$
BEGIN
 IF OLD.cleaned_at IS NULL AND NEW.cleaned_at IS NOT NULL THEN
  UPDATE public.access_requests SET reason=NULL WHERE requester=NEW.id AND reason IS NOT NULL
   AND NOT EXISTS(SELECT FROM public.legal_holds h WHERE h.released_at IS NULL AND (h.user_id=NEW.id OR h.workspace_id=access_requests.workspace_id));
 END IF;
 RETURN NEW;
END $$;
REVOKE ALL ON FUNCTION access_request_reason_cleanup() FROM PUBLIC;
CREATE TRIGGER access_request_reason_cleanup AFTER UPDATE OF cleaned_at ON users FOR EACH ROW EXECUTE FUNCTION access_request_reason_cleanup();

-- Resume erasure for already-cleaned users after a workspace hold ends.
-- No caller-selected IDs/text and no general UPDATE(reason) capability.
CREATE FUNCTION scrub_access_request_reasons() RETURNS bigint LANGUAGE plpgsql SECURITY DEFINER SET search_path=public,pg_temp AS $$
DECLARE scrubbed bigint;
BEGIN
 -- Also safe if invoked directly by runtime: serialize with hold placement
 -- and cleanup through their normal catalog/installation lock protocol.
 -- A snapshot fixed before a lock wait cannot safely recheck a newer hold.
 IF current_setting('transaction_isolation')<>'read committed' THEN
  RAISE EXCEPTION 'request reason cleanup requires READ COMMITTED' USING ERRCODE='25000';
 END IF;
 PERFORM pg_advisory_xact_lock_shared(72419502);
 PERFORM public.lock_installation();
 WITH candidates AS (
  SELECT r.id FROM public.access_requests r JOIN public.users u ON u.id=r.requester
  WHERE u.cleaned_at IS NOT NULL AND r.reason IS NOT NULL
   AND NOT EXISTS(SELECT FROM public.legal_holds h WHERE h.released_at IS NULL AND (h.user_id=r.requester OR h.workspace_id=r.workspace_id))
  ORDER BY r.created_at,r.id LIMIT 100 FOR UPDATE OF r SKIP LOCKED
 )
 UPDATE public.access_requests SET reason=NULL WHERE id IN (SELECT id FROM candidates);
 GET DIAGNOSTICS scrubbed=ROW_COUNT;
 RETURN scrubbed;
END $$;
REVOKE ALL ON FUNCTION scrub_access_request_reasons() FROM PUBLIC;
