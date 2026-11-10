-- #90 slice 6: keys of shared workspaces as access review subjects. A
-- campaign may cover `workspace_keys`: one item per live key lineage (a
-- rotation keeps the lineage, so rotating does not end the review of a
-- credential) of an enabled Team or Project, human and service-account keys,
-- never a personal workspace's. The item records the key at the snapshot,
-- its lineage, workspace and holder (the person for a human key, the service
-- account otherwise), derived by the database. Reviewers are the workspace's
-- owners and admins, never the key's own holder; Platform Admins are not key
-- reviewers (they have no key authority in a workspace they do not
-- administer). "remove" revokes every key of the lineage through the direct
-- revoke path; a deferred check refuses a remove decision otherwise.
ALTER TABLE access_review_campaigns DROP CONSTRAINT access_review_campaigns_subjects_check;
ALTER TABLE access_review_campaigns ADD CONSTRAINT access_review_campaigns_subjects_check CHECK (cardinality(subjects) BETWEEN 1 AND 3 AND subjects <@ ARRAY['workspace_memberships','platform_roles','workspace_keys']::text[] AND array_position(subjects, NULL) IS NULL);

-- As in 0076, with the workspace filter narrowing memberships and/or keys.
CREATE OR REPLACE FUNCTION access_review_campaigns_guard() RETURNS trigger LANGUAGE plpgsql SET search_path = pg_catalog, public AS $$
BEGIN
 IF TG_OP = 'INSERT' THEN
  NEW.created_at := now();
  NEW.created_xid := pg_current_xact_id();
  IF NOT EXISTS (SELECT FROM public.effective_platform_roles p WHERE p.user_id = NEW.created_by AND p.role = 'admin')
     OR NEW.closed_at IS NOT NULL OR NEW.closed_by IS NOT NULL
     OR cardinality(NEW.subjects) <> (SELECT count(DISTINCT s) FROM unnest(NEW.subjects) s)
     OR cardinality(NEW.workspace_ids) <> (SELECT count(DISTINCT w) FROM unnest(NEW.workspace_ids) w)
     OR (cardinality(NEW.workspace_ids) > 0 AND NOT NEW.subjects && ARRAY['workspace_memberships','workspace_keys']::text[])
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

-- Key items: the key at the snapshot, its lineage and service account (all
-- derived); no person for a service key, no role or source for any key.
ALTER TABLE access_review_items
 ADD COLUMN api_key_id uuid,
 ADD COLUMN key_lineage_id uuid,
 ADD COLUMN service_account_id uuid,
 ALTER COLUMN user_id DROP NOT NULL,
 ALTER COLUMN role DROP NOT NULL,
 ALTER COLUMN source DROP NOT NULL;
ALTER TABLE access_review_items DROP CONSTRAINT access_review_items_subject_kind_check;
ALTER TABLE access_review_items
 ADD CONSTRAINT access_review_items_subject_kind_check CHECK (subject_kind IN ('workspace_membership','platform_role','workspace_key')),
 ADD CONSTRAINT access_review_items_shape CHECK (CASE WHEN subject_kind = 'workspace_key'
   THEN api_key_id IS NOT NULL AND key_lineage_id IS NOT NULL AND workspace_id IS NOT NULL
        AND membership_grant_id IS NULL AND platform_grant_id IS NULL AND role IS NULL AND source IS NULL
        AND (user_id IS NULL) <> (service_account_id IS NULL)
   ELSE api_key_id IS NULL AND key_lineage_id IS NULL AND service_account_id IS NULL
        AND user_id IS NOT NULL AND role IS NOT NULL AND source IS NOT NULL END),
 ADD CONSTRAINT access_review_items_key FOREIGN KEY (workspace_id, api_key_id) REFERENCES api_keys(workspace_id, id),
 ADD CONSTRAINT access_review_items_service_account FOREIGN KEY (workspace_id, service_account_id) REFERENCES service_accounts(workspace_id, id),
 ADD CONSTRAINT access_review_items_campaign_lineage UNIQUE (campaign_id, key_lineage_id);

CREATE OR REPLACE FUNCTION access_review_items_guard() RETURNS trigger LANGUAGE plpgsql SET search_path = pg_catalog, public AS $$
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
  IF FOUND AND NEW.platform_grant_id IS NULL AND NEW.api_key_id IS NULL THEN RETURN NEW; END IF;
 ELSIF NEW.subject_kind = 'platform_role' AND 'platform_roles' = ANY (subjects) THEN
  SELECT NULL, g.user_id, g.role, g.source, g.created_at
    INTO NEW.workspace_id, NEW.user_id, NEW.role, NEW.source, NEW.granted_at
    FROM public.platform_role_grants g
    JOIN public.users u ON u.id = g.user_id
   WHERE g.id = NEW.platform_grant_id AND g.revoked_at IS NULL AND g.role IN ('admin','auditor')
     AND u.disabled_at IS NULL AND u.cleaned_at IS NULL;
  IF FOUND AND NEW.membership_grant_id IS NULL AND NEW.api_key_id IS NULL THEN RETURN NEW; END IF;
 ELSIF NEW.subject_kind = 'workspace_key' AND 'workspace_keys' = ANY (subjects) THEN
  -- A live key (not revoked, not expired; a disabled key can be enabled
  -- again, so it is reviewed) of an enabled Team/Project in the filter; a
  -- human key's holder neither disabled nor cleaned.
  SELECT k.workspace_id, k.issued_to_user_id, k.service_account_id, k.governance_key_id, NULL, NULL, k.created_at
    INTO NEW.workspace_id, NEW.user_id, NEW.service_account_id, NEW.key_lineage_id, NEW.role, NEW.source, NEW.granted_at
    FROM public.api_keys k
    JOIN public.workspaces w ON w.id = k.workspace_id
    LEFT JOIN public.users u ON u.id = k.issued_to_user_id
   WHERE k.id = NEW.api_key_id AND k.revoked_at IS NULL AND (k.expires_at IS NULL OR k.expires_at > now())
     AND w.kind IN ('team','project') AND w.disabled_at IS NULL
     AND (cardinality(filter) = 0 OR w.id = ANY (filter))
     AND (k.issued_to_user_id IS NULL OR (u.disabled_at IS NULL AND u.cleaned_at IS NULL));
  IF FOUND AND NEW.membership_grant_id IS NULL AND NEW.platform_grant_id IS NULL THEN RETURN NEW; END IF;
 END IF;
 RAISE EXCEPTION 'an access review item names a live grant the campaign covers' USING ERRCODE = '23514';
END $$;

-- In effect: memberships and platform roles as in 0077; a key item while a
-- key of its lineage is neither revoked nor expired. The person (if any)
-- neither disabled nor cleaned up; the workspace (if any) enabled.
CREATE OR REPLACE FUNCTION access_review_item_active(item uuid) RETURNS boolean LANGUAGE sql STABLE SET search_path = pg_catalog, public AS $$
 SELECT EXISTS (
  SELECT FROM public.access_review_items i
  LEFT JOIN public.users u ON u.id = i.user_id
  LEFT JOIN public.workspaces w ON w.id = i.workspace_id
  LEFT JOIN public.workspace_membership_grants m ON m.id = i.membership_grant_id
  LEFT JOIN public.platform_role_grants p ON p.id = i.platform_grant_id
  WHERE i.id = item
    AND CASE WHEN i.subject_kind = 'workspace_key'
             THEN EXISTS (SELECT FROM public.api_keys k WHERE k.workspace_id = i.workspace_id AND k.governance_key_id = i.key_lineage_id
                          AND k.revoked_at IS NULL AND (k.expires_at IS NULL OR k.expires_at > now()))
             ELSE coalesce(m.revoked_at, p.revoked_at) IS NULL END
    AND (i.user_id IS NULL OR (u.disabled_at IS NULL AND u.cleaned_at IS NULL))
    AND (w.id IS NULL OR w.disabled_at IS NULL))
$$;

-- A candidate reviewer: never the item's own person (NULL-safe: a service
-- key has none); Platform Admins for memberships and platform roles; the
-- workspace's owners and admins for memberships and keys.
CREATE OR REPLACE FUNCTION access_review_candidate(item uuid, reviewer uuid) RETURNS boolean LANGUAGE sql STABLE SET search_path = pg_catalog, public AS $$
 SELECT EXISTS (
  SELECT FROM public.access_review_items i
  WHERE i.id = item AND i.user_id IS DISTINCT FROM reviewer
    AND ((i.subject_kind <> 'workspace_key' AND EXISTS (SELECT FROM public.effective_platform_roles p WHERE p.user_id = reviewer AND p.role = 'admin'))
         OR (i.subject_kind <> 'platform_role' AND EXISTS (
              SELECT FROM public.effective_workspace_memberships e
              WHERE e.workspace_id = i.workspace_id AND e.user_id = reviewer AND e.role IN ('owner','admin')))))
$$;

CREATE OR REPLACE FUNCTION access_review_may_decide(item uuid, reviewer uuid) RETURNS boolean LANGUAGE sql STABLE SET search_path = pg_catalog, public AS $$
 SELECT EXISTS (
  SELECT FROM public.access_review_items i
  JOIN public.access_review_campaigns c ON c.id = i.campaign_id AND c.closed_at IS NULL
  LEFT JOIN LATERAL (SELECT a.reviewer_user_id FROM public.access_review_assignments a
                     WHERE a.item_id = i.id ORDER BY a.seq DESC LIMIT 1) a ON true
  WHERE i.id = item AND i.user_id IS DISTINCT FROM reviewer
    AND EXISTS (SELECT FROM public.effective_platform_roles p WHERE p.user_id = reviewer)
    AND CASE WHEN a.reviewer_user_id IS NOT NULL AND public.access_review_candidate(i.id, a.reviewer_user_id)
             THEN a.reviewer_user_id = reviewer
             WHEN i.subject_kind <> 'platform_role'
             THEN EXISTS (SELECT FROM public.effective_workspace_memberships e
                          WHERE e.workspace_id = i.workspace_id AND e.user_id = reviewer AND e.role IN ('owner','admin'))
             ELSE EXISTS (SELECT FROM public.effective_platform_roles p WHERE p.user_id = reviewer AND p.role = 'admin') END)
$$;

-- As in 0078; a key item may be removed whatever its holder.
CREATE OR REPLACE FUNCTION access_review_decisions_guard() RETURNS trigger LANGUAGE plpgsql SET search_path = pg_catalog, public AS $$
BEGIN
 PERFORM 1 FROM public.access_review_campaigns c JOIN public.access_review_items i ON i.campaign_id = c.id
  WHERE i.id = NEW.item_id FOR SHARE OF c;
 NEW.decided_at := clock_timestamp();
 PERFORM pg_advisory_xact_lock(hashtextextended('access_review_decisions:' || NEW.item_id::text, 0));
 NEW.seq := coalesce((SELECT max(x.seq) FROM public.access_review_decisions x WHERE x.item_id = NEW.item_id), 0) + 1;
 IF NOT public.access_review_may_decide(NEW.item_id, NEW.decided_by)
    OR NOT public.access_review_item_active(NEW.item_id)
    OR (NEW.decision = 'flag_idp' AND NOT EXISTS (SELECT FROM public.access_review_items i WHERE i.id = NEW.item_id AND i.source = 'group'))
    OR (NEW.decision = 'remove' AND NOT EXISTS (SELECT FROM public.access_review_items i WHERE i.id = NEW.item_id
                                                AND (i.source IN ('manual','bootstrap') OR i.subject_kind = 'workspace_key')))
    OR (NEW.decision = 'remove' AND EXISTS (
         SELECT FROM public.access_review_items i
         WHERE i.id = NEW.item_id AND i.subject_kind = 'workspace_membership' AND i.role = 'owner'
           AND NOT EXISTS (SELECT FROM public.effective_workspace_memberships e
                           WHERE e.workspace_id = i.workspace_id AND e.user_id = NEW.decided_by AND e.role = 'owner')
           AND NOT EXISTS (SELECT FROM public.effective_platform_roles p WHERE p.user_id = NEW.decided_by AND p.role = 'admin'))) THEN
  RAISE EXCEPTION 'an access review decision is made by a reviewer of the item while it is open and in effect' USING ERRCODE = '23514';
 END IF;
 RETURN NEW;
END $$;

-- At commit: a membership or role item's own grant revoked (as in 0078); a
-- key item's whole lineage revoked (not merely expired).
CREATE OR REPLACE FUNCTION access_review_removal_applied() RETURNS trigger LANGUAGE plpgsql SET search_path = pg_catalog, public AS $$
BEGIN
 IF NEW.decision = 'remove' AND NOT EXISTS (
     SELECT FROM public.access_review_items i
     LEFT JOIN public.workspace_membership_grants m ON m.id = i.membership_grant_id
     LEFT JOIN public.platform_role_grants p ON p.id = i.platform_grant_id
     WHERE i.id = NEW.item_id
       AND CASE WHEN i.subject_kind = 'workspace_key'
                THEN NOT EXISTS (SELECT FROM public.api_keys k WHERE k.workspace_id = i.workspace_id
                                 AND k.governance_key_id = i.key_lineage_id AND k.revoked_at IS NULL)
                ELSE coalesce(m.revoked_at, p.revoked_at) IS NOT NULL END) THEN
  RAISE EXCEPTION 'a remove decision is recorded only with the removal of the access' USING ERRCODE = '23514';
 END IF;
 RETURN NULL;
END $$;
