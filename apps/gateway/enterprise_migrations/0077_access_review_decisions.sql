-- #90 slice 2: reviewer decisions and reviewer assignments for access review
-- items. Both are append-only; the latest row per item counts. Who may
-- decide is derived live (access_review_may_decide), the single rule the API
-- and the guards share: the item's campaign is open; the reviewer is
-- entitled and is not the item's own person; if a Platform Admin assigned
-- the item to someone who is still a candidate, only that reviewer;
-- otherwise (no assignment, or the assignee no longer qualifies) a current
-- owner/admin of the item's workspace (membership items) or a current
-- Platform Admin (platform role items). The latest row per item is the one
-- with the highest seq (a per-item ordinal set by the guard under a per-item
-- lock, so the wall clock never reorders). A candidate for assignment
-- is a current owner/admin of the item's workspace or a Platform Admin
-- (platform role items: a Platform Admin), never the item's own person.

-- An item's grant is still in effect: not revoked, its person neither
-- disabled nor cleaned up, its workspace (if any) not disabled.
CREATE FUNCTION access_review_item_active(item uuid) RETURNS boolean LANGUAGE sql STABLE SET search_path = pg_catalog, public AS $$
 SELECT EXISTS (
  SELECT FROM public.access_review_items i
  JOIN public.users u ON u.id = i.user_id
  LEFT JOIN public.workspaces w ON w.id = i.workspace_id
  LEFT JOIN public.workspace_membership_grants m ON m.id = i.membership_grant_id
  LEFT JOIN public.platform_role_grants p ON p.id = i.platform_grant_id
  WHERE i.id = item AND coalesce(m.revoked_at, p.revoked_at) IS NULL
    AND u.disabled_at IS NULL AND u.cleaned_at IS NULL AND (w.id IS NULL OR w.disabled_at IS NULL))
$$;

CREATE FUNCTION access_review_candidate(item uuid, reviewer uuid) RETURNS boolean LANGUAGE sql STABLE SET search_path = pg_catalog, public AS $$
 SELECT EXISTS (
  SELECT FROM public.access_review_items i
  WHERE i.id = item AND i.user_id <> reviewer
    AND (EXISTS (SELECT FROM public.effective_platform_roles p WHERE p.user_id = reviewer AND p.role = 'admin')
         OR (i.subject_kind = 'workspace_membership' AND EXISTS (
              SELECT FROM public.effective_workspace_memberships e
              WHERE e.workspace_id = i.workspace_id AND e.user_id = reviewer AND e.role IN ('owner','admin')))))
$$;

CREATE TABLE access_review_assignments (
 id uuid PRIMARY KEY,
 item_id uuid NOT NULL REFERENCES access_review_items(id),
 -- NULL: back to the automatic reviewers.
 reviewer_user_id uuid REFERENCES users(id),
 assigned_by uuid NOT NULL REFERENCES users(id),
 assigned_at timestamptz NOT NULL DEFAULT clock_timestamp(),
 seq integer NOT NULL,
 UNIQUE (item_id, seq)
);
CREATE INDEX access_review_assignments_reviewer ON access_review_assignments (reviewer_user_id) WHERE reviewer_user_id IS NOT NULL;

CREATE FUNCTION access_review_may_decide(item uuid, reviewer uuid) RETURNS boolean LANGUAGE sql STABLE SET search_path = pg_catalog, public AS $$
 SELECT EXISTS (
  SELECT FROM public.access_review_items i
  JOIN public.access_review_campaigns c ON c.id = i.campaign_id AND c.closed_at IS NULL
  LEFT JOIN LATERAL (SELECT a.reviewer_user_id FROM public.access_review_assignments a
                     WHERE a.item_id = i.id ORDER BY a.seq DESC LIMIT 1) a ON true
  WHERE i.id = item AND i.user_id <> reviewer
    AND EXISTS (SELECT FROM public.effective_platform_roles p WHERE p.user_id = reviewer)
    AND CASE WHEN a.reviewer_user_id IS NOT NULL AND public.access_review_candidate(i.id, a.reviewer_user_id)
             THEN a.reviewer_user_id = reviewer
             WHEN i.subject_kind = 'workspace_membership'
             THEN EXISTS (SELECT FROM public.effective_workspace_memberships e
                          WHERE e.workspace_id = i.workspace_id AND e.user_id = reviewer AND e.role IN ('owner','admin'))
             ELSE EXISTS (SELECT FROM public.effective_platform_roles p WHERE p.user_id = reviewer AND p.role = 'admin') END)
$$;

CREATE TABLE access_review_decisions (
 id uuid PRIMARY KEY,
 item_id uuid NOT NULL REFERENCES access_review_items(id),
 -- remove arrives with the slice that applies it.
 decision text NOT NULL CHECK (decision IN ('keep','flag_idp')),
 decided_by uuid NOT NULL REFERENCES users(id),
 decided_at timestamptz NOT NULL DEFAULT clock_timestamp(),
 seq integer NOT NULL,
 UNIQUE (item_id, seq)
);
-- The inbox's candidate rows by workspace.
CREATE INDEX access_review_items_workspace ON access_review_items (workspace_id, campaign_id) WHERE workspace_id IS NOT NULL;

-- A decision: by someone who may decide the item now, on an item still in
-- effect, flag_idp only for group-provided access; server clock. The
-- campaign row is share-locked first, so a decision and closing the campaign
-- serialize (closing takes the row for update).
CREATE FUNCTION access_review_decisions_guard() RETURNS trigger LANGUAGE plpgsql SET search_path = pg_catalog, public AS $$
BEGIN
 PERFORM 1 FROM public.access_review_campaigns c JOIN public.access_review_items i ON i.campaign_id = c.id
  WHERE i.id = NEW.item_id FOR SHARE OF c;
 NEW.decided_at := clock_timestamp();
 PERFORM pg_advisory_xact_lock(hashtextextended('access_review_decisions:' || NEW.item_id::text, 0));
 NEW.seq := coalesce((SELECT max(x.seq) FROM public.access_review_decisions x WHERE x.item_id = NEW.item_id), 0) + 1;
 IF NOT public.access_review_may_decide(NEW.item_id, NEW.decided_by)
    OR NOT public.access_review_item_active(NEW.item_id)
    OR (NEW.decision = 'flag_idp' AND NOT EXISTS (SELECT FROM public.access_review_items i WHERE i.id = NEW.item_id AND i.source = 'group')) THEN
  RAISE EXCEPTION 'an access review decision is made by a reviewer of the item while it is open and in effect' USING ERRCODE = '23514';
 END IF;
 RETURN NEW;
END $$;
CREATE TRIGGER access_review_decisions_guard BEFORE INSERT ON access_review_decisions FOR EACH ROW EXECUTE FUNCTION access_review_decisions_guard();
CREATE TRIGGER access_review_decisions_immutable BEFORE UPDATE OR DELETE ON access_review_decisions FOR EACH ROW EXECUTE FUNCTION immutable_history();
CREATE TRIGGER access_review_decisions_no_truncate BEFORE TRUNCATE ON access_review_decisions FOR EACH STATEMENT EXECUTE FUNCTION immutable_history();
REVOKE ALL ON FUNCTION access_review_decisions_guard() FROM PUBLIC;

-- An assignment: by a current Platform Admin who is not the person under
-- review, while the campaign is open, to a candidate reviewer (or NULL);
-- server clock; serialized with closing as decisions are.
CREATE FUNCTION access_review_assignments_guard() RETURNS trigger LANGUAGE plpgsql SET search_path = pg_catalog, public AS $$
BEGIN
 PERFORM 1 FROM public.access_review_campaigns c JOIN public.access_review_items i ON i.campaign_id = c.id
  WHERE i.id = NEW.item_id FOR SHARE OF c;
 NEW.assigned_at := clock_timestamp();
 PERFORM pg_advisory_xact_lock(hashtextextended('access_review_assignments:' || NEW.item_id::text, 0));
 NEW.seq := coalesce((SELECT max(y.seq) FROM public.access_review_assignments y WHERE y.item_id = NEW.item_id), 0) + 1;
 IF NOT EXISTS (SELECT FROM public.effective_platform_roles p WHERE p.user_id = NEW.assigned_by AND p.role = 'admin')
    OR EXISTS (SELECT FROM public.access_review_items i WHERE i.id = NEW.item_id AND i.user_id = NEW.assigned_by)
    OR NOT EXISTS (SELECT FROM public.access_review_items i JOIN public.access_review_campaigns c ON c.id = i.campaign_id
                   WHERE i.id = NEW.item_id AND c.closed_at IS NULL)
    OR (NEW.reviewer_user_id IS NOT NULL AND NOT public.access_review_candidate(NEW.item_id, NEW.reviewer_user_id)) THEN
  RAISE EXCEPTION 'an access review item is assigned by a Platform Admin to a candidate reviewer' USING ERRCODE = '23514';
 END IF;
 RETURN NEW;
END $$;
CREATE TRIGGER access_review_assignments_guard BEFORE INSERT ON access_review_assignments FOR EACH ROW EXECUTE FUNCTION access_review_assignments_guard();
CREATE TRIGGER access_review_assignments_immutable BEFORE UPDATE OR DELETE ON access_review_assignments FOR EACH ROW EXECUTE FUNCTION immutable_history();
CREATE TRIGGER access_review_assignments_no_truncate BEFORE TRUNCATE ON access_review_assignments FOR EACH STATEMENT EXECUTE FUNCTION immutable_history();
REVOKE ALL ON FUNCTION access_review_assignments_guard() FROM PUBLIC;
REVOKE ALL ON FUNCTION access_review_item_active(uuid), access_review_candidate(uuid,uuid), access_review_may_decide(uuid,uuid) FROM PUBLIC;
