-- #90 slice 3: a reviewer's "remove" decision. The gateway removes the access
-- in the same transaction through the existing removal paths (membership:
-- the members path; platform role: the role path), so a remove decision is
-- recorded only with the removal: a deferred check refuses any remove
-- decision whose access is still in effect at commit. remove applies only to
-- manual or bootstrap access (group-provided access is flagged for the
-- identity provider), and an owner's membership only by an owner of that
-- workspace or a Platform Admin, as on the members routes.
ALTER TABLE access_review_decisions DROP CONSTRAINT access_review_decisions_decision_check;
ALTER TABLE access_review_decisions ADD CONSTRAINT access_review_decisions_decision_check CHECK (decision IN ('keep','flag_idp','remove'));

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
    OR (NEW.decision = 'remove' AND NOT EXISTS (SELECT FROM public.access_review_items i WHERE i.id = NEW.item_id AND i.source IN ('manual','bootstrap')))
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

CREATE FUNCTION access_review_removal_applied() RETURNS trigger LANGUAGE plpgsql SET search_path = pg_catalog, public AS $$
BEGIN
 IF NEW.decision = 'remove' AND public.access_review_item_active(NEW.item_id) THEN
  RAISE EXCEPTION 'a remove decision is recorded only with the removal of the access' USING ERRCODE = '23514';
 END IF;
 RETURN NULL;
END $$;
CREATE CONSTRAINT TRIGGER access_review_removal_applied AFTER INSERT ON access_review_decisions
 DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION access_review_removal_applied();
REVOKE ALL ON FUNCTION access_review_removal_applied() FROM PUBLIC;
