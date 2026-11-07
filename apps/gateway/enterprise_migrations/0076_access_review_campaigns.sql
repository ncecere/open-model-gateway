-- #90: access review campaigns. A Platform Admin starts a one-shot campaign
-- that snapshots the live shared (Team/Project) memberships and/or elevated
-- platform role grants (Admin, Auditor) it covers. Items are derived from
-- their grant by the database (the runtime names only the grant), added only
-- in the transaction that creates the campaign, and never change. Nothing is
-- deleted. Later slices record reviewer decisions and reviewer overrides in
-- their own append-only tables (items stay immutable; reviewers are derived
-- live from current owners/admins), and long-lived keys as another subject.
CREATE TABLE access_review_campaigns (
 id uuid PRIMARY KEY,
 -- Operator labels, immutable: never personal data. Non-blank, no control characters.
 name text NOT NULL CHECK (char_length(name) BETWEEN 1 AND 120 AND btrim(name) <> '' AND name !~ '[\x01-\x1f\x7f-\x9f]'),
 subjects text[] NOT NULL CHECK (cardinality(subjects) BETWEEN 1 AND 2 AND subjects <@ ARRAY['workspace_memberships','platform_roles']::text[] AND array_position(subjects, NULL) IS NULL),
 -- Narrows workspace memberships to these Teams/Projects; empty means all.
 workspace_ids uuid[] NOT NULL DEFAULT '{}' CHECK (cardinality(workspace_ids) <= 50 AND array_position(workspace_ids, NULL) IS NULL),
 created_by uuid NOT NULL REFERENCES users(id),
 created_at timestamptz NOT NULL DEFAULT now(),
 due_at timestamptz NOT NULL,
 closed_at timestamptz,
 closed_by uuid REFERENCES users(id),
 -- The creating transaction (set by the guard): items are accepted only there.
 created_xid xid8 NOT NULL,
 CHECK (due_at >= created_at + interval '1 day' AND due_at <= created_at + interval '90 days'),
 CHECK ((closed_at IS NULL) = (closed_by IS NULL))
);
CREATE INDEX access_review_campaigns_open ON access_review_campaigns (created_at) WHERE closed_at IS NULL;

-- Started by a current Platform Admin on the server clock (the transaction's,
-- so the due date set in the same statement is measured from it); repeated
-- subjects or filter entries are refused; only closing changes a campaign,
-- once, by a current Platform Admin.
CREATE FUNCTION access_review_campaigns_guard() RETURNS trigger LANGUAGE plpgsql SET search_path = pg_catalog, public AS $$
BEGIN
 IF TG_OP = 'INSERT' THEN
  NEW.created_at := now();
  NEW.created_xid := pg_current_xact_id();
  IF NOT EXISTS (SELECT FROM public.effective_platform_roles p WHERE p.user_id = NEW.created_by AND p.role = 'admin')
     OR NEW.closed_at IS NOT NULL OR NEW.closed_by IS NOT NULL
     OR cardinality(NEW.subjects) <> (SELECT count(DISTINCT s) FROM unnest(NEW.subjects) s)
     OR cardinality(NEW.workspace_ids) <> (SELECT count(DISTINCT w) FROM unnest(NEW.workspace_ids) w)
     OR (cardinality(NEW.workspace_ids) > 0 AND NOT 'workspace_memberships' = ANY (NEW.subjects))
     OR (SELECT count(*) FROM public.workspaces w WHERE w.id = ANY (NEW.workspace_ids) AND w.kind IN ('team','project')) <> cardinality(NEW.workspace_ids) THEN
   RAISE EXCEPTION 'an access review campaign is started by a Platform Admin over shared workspaces' USING ERRCODE = '23514';
  END IF;
  RETURN NEW;
 END IF;
 IF (NEW.id, NEW.name, NEW.subjects, NEW.workspace_ids, NEW.created_by, NEW.created_at, NEW.due_at, NEW.created_xid)
    IS DISTINCT FROM (OLD.id, OLD.name, OLD.subjects, OLD.workspace_ids, OLD.created_by, OLD.created_at, OLD.due_at, OLD.created_xid)
    OR OLD.closed_at IS NOT NULL OR NEW.closed_at IS NULL
    OR NOT EXISTS (SELECT FROM public.effective_platform_roles p WHERE p.user_id = NEW.closed_by AND p.role = 'admin') THEN
  RAISE EXCEPTION 'access review campaigns are immutable; closing happens once' USING ERRCODE = '23514';
 END IF;
 NEW.closed_at := clock_timestamp();
 RETURN NEW;
END $$;
CREATE TRIGGER access_review_campaigns_guard BEFORE INSERT OR UPDATE ON access_review_campaigns FOR EACH ROW EXECUTE FUNCTION access_review_campaigns_guard();
CREATE TRIGGER access_review_campaigns_no_delete BEFORE DELETE ON access_review_campaigns FOR EACH ROW EXECUTE FUNCTION immutable_history();
CREATE TRIGGER access_review_campaigns_no_truncate BEFORE TRUNCATE ON access_review_campaigns FOR EACH STATEMENT EXECUTE FUNCTION immutable_history();
REVOKE ALL ON FUNCTION access_review_campaigns_guard() FROM PUBLIC;

CREATE TABLE access_review_items (
 id uuid PRIMARY KEY,
 campaign_id uuid NOT NULL REFERENCES access_review_campaigns(id),
 subject_kind text NOT NULL CHECK (subject_kind IN ('workspace_membership','platform_role')),
 membership_grant_id uuid REFERENCES workspace_membership_grants(id),
 platform_grant_id uuid REFERENCES platform_role_grants(id),
 -- Derived from the grant by the guard below.
 workspace_id uuid REFERENCES workspaces(id),
 user_id uuid NOT NULL REFERENCES users(id),
 role text NOT NULL CHECK (role IN ('owner','admin','member','auditor')),
 source text NOT NULL CHECK (source IN ('manual','group','bootstrap')),
 granted_at timestamptz NOT NULL,
 CHECK ((subject_kind = 'workspace_membership') = (membership_grant_id IS NOT NULL AND workspace_id IS NOT NULL)),
 CHECK ((subject_kind = 'platform_role') = (platform_grant_id IS NOT NULL)),
 CHECK (membership_grant_id IS NULL OR platform_grant_id IS NULL),
 UNIQUE (campaign_id, membership_grant_id),
 UNIQUE (campaign_id, platform_grant_id)
);
CREATE INDEX access_review_items_campaign ON access_review_items (campaign_id, subject_kind, id);

-- An item names one live grant of a kind the campaign covers, in the
-- transaction that created the campaign; the workspace, user, role, source
-- and grant time are copied from the grant, whatever the insert said.
-- Memberships: an enabled Team/Project (in the campaign's filter, if any)
-- and a user who is neither disabled nor cleaned. Platform roles: an Admin or
-- Auditor grant (the User entitlement is not reviewed here), same user rule.
-- The campaign must be open and created by this transaction (a 64-bit id,
-- so wraparound cannot match an old campaign).
CREATE FUNCTION access_review_items_guard() RETURNS trigger LANGUAGE plpgsql SET search_path = pg_catalog, public AS $$
DECLARE
 subjects text[];
 filter uuid[];
 creator xid8;
 closed timestamptz;
BEGIN
 SELECT c.subjects, c.workspace_ids, c.created_xid, c.closed_at INTO subjects, filter, creator, closed
   FROM public.access_review_campaigns c WHERE c.id = NEW.campaign_id;
 IF NOT FOUND OR creator <> pg_current_xact_id() OR closed IS NOT NULL THEN
  RAISE EXCEPTION 'access review items are added only with their campaign' USING ERRCODE = '23514';
 END IF;
 IF NEW.subject_kind = 'workspace_membership' AND 'workspace_memberships' = ANY (subjects) THEN
  SELECT g.workspace_id, g.user_id, g.role, g.source, g.created_at
    INTO NEW.workspace_id, NEW.user_id, NEW.role, NEW.source, NEW.granted_at
    FROM public.workspace_membership_grants g
    JOIN public.workspaces w ON w.id = g.workspace_id
    JOIN public.users u ON u.id = g.user_id
   WHERE g.id = NEW.membership_grant_id AND g.revoked_at IS NULL
     AND w.kind IN ('team','project') AND w.disabled_at IS NULL
     AND (cardinality(filter) = 0 OR w.id = ANY (filter))
     AND u.disabled_at IS NULL AND u.cleaned_at IS NULL;
  IF FOUND AND NEW.platform_grant_id IS NULL THEN RETURN NEW; END IF;
 ELSIF NEW.subject_kind = 'platform_role' AND 'platform_roles' = ANY (subjects) THEN
  SELECT NULL, g.user_id, g.role, g.source, g.created_at
    INTO NEW.workspace_id, NEW.user_id, NEW.role, NEW.source, NEW.granted_at
    FROM public.platform_role_grants g
    JOIN public.users u ON u.id = g.user_id
   WHERE g.id = NEW.platform_grant_id AND g.revoked_at IS NULL AND g.role IN ('admin','auditor')
     AND u.disabled_at IS NULL AND u.cleaned_at IS NULL;
  IF FOUND AND NEW.membership_grant_id IS NULL THEN RETURN NEW; END IF;
 END IF;
 RAISE EXCEPTION 'an access review item names a live grant the campaign covers' USING ERRCODE = '23514';
END $$;
CREATE TRIGGER access_review_items_guard BEFORE INSERT ON access_review_items FOR EACH ROW EXECUTE FUNCTION access_review_items_guard();
CREATE TRIGGER access_review_items_immutable BEFORE UPDATE OR DELETE ON access_review_items FOR EACH ROW EXECUTE FUNCTION immutable_history();
CREATE TRIGGER access_review_items_no_truncate BEFORE TRUNCATE ON access_review_items FOR EACH STATEMENT EXECUTE FUNCTION immutable_history();
REVOKE ALL ON FUNCTION access_review_items_guard() FROM PUBLIC;
