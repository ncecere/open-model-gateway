-- #101 C. Explicit operator upgrade only. As in 0049, BOTH source fields
-- stay NULL: pre-0058 feeds, history and recipient selectors cannot match
-- these incidents. An old lease holder does not evaluate them; an old email
-- worker may consume a fired delivery with no recipients (best effort).
-- No reservation/ledger/totals DDL or scans; alert_events is not admission's
-- hot path. NOT VALID avoids scanning history under the ALTER lock: all old
-- rows already satisfy the original, stricter source/kind constraints, and
-- the new kind-specific constraint is vacuous for them. New writes are checked.
SET lock_timeout='60s';
ALTER TABLE alert_events DROP CONSTRAINT alert_events_kind_check;
ALTER TABLE alert_events ADD CONSTRAINT alert_events_kind_check CHECK(kind IN (
 'budget_threshold','spend_spike','error_rate','provider_failing','scim_last_admin',
 'batch_failed','batch_stalled','spend_threshold','partitions_missing','history_orphans',
 'admission_ceiling','key_expiring','aged_unknown_hold')) NOT VALID;
ALTER TABLE alert_events DROP CONSTRAINT alert_events_check;
ALTER TABLE alert_events ADD CONSTRAINT alert_events_check CHECK(
 (kind IN ('key_expiring','aged_unknown_hold') AND rule_id IS NULL AND builtin IS NULL)
 OR (kind NOT IN ('key_expiring','aged_unknown_hold') AND (rule_id IS NULL) <> (builtin IS NULL))) NOT VALID;
ALTER TABLE alert_events ADD CONSTRAINT alert_events_aged_shape CHECK(kind<>'aged_unknown_hold' OR (
 level IN (1,2) AND provider_connection_id IS NULL
 AND ((workspace_id IS NULL AND subject_key='personal')
 OR (workspace_id IS NOT NULL AND subject_key='workspace:'||workspace_id::text))
 AND severity=CASE level WHEN 1 THEN 'warning' ELSE 'critical' END
 AND details=jsonb_build_object('threshold_hours',CASE level WHEN 1 THEN 24 ELSE 168 END)
 AND summary=CASE level WHEN 1 THEN 'Holds with unknown cost are over 24 hours old' ELSE 'Holds with unknown cost are over 168 hours old' END
)) NOT VALID;

CREATE FUNCTION alert_event_aged_scope() RETURNS trigger LANGUAGE plpgsql SET search_path = pg_catalog, public, pg_temp AS $$
BEGIN
 IF NEW.kind='aged_unknown_hold' AND NEW.workspace_id IS NOT NULL
 AND NOT EXISTS(SELECT FROM public.workspaces w WHERE w.id=NEW.workspace_id AND w.kind IN ('team','project')) THEN
  RAISE EXCEPTION 'aged unknown hold incident requires a shared workspace or installation aggregate' USING ERRCODE='23514';
 END IF;
 RETURN NEW;
END $$;
CREATE TRIGGER alert_event_aged_scope BEFORE INSERT ON alert_events FOR EACH ROW EXECUTE FUNCTION alert_event_aged_scope();

-- Only alert history is indexed, never the partitioned admission tables.
CREATE INDEX alert_events_aged_subject_fired ON alert_events(subject_key,fired_at) WHERE kind='aged_unknown_hold';
CREATE INDEX alert_events_aged_fired ON alert_events(fired_at) WHERE kind='aged_unknown_hold';
CREATE INDEX alert_events_aged_open ON alert_events(fired_at,id) WHERE kind='aged_unknown_hold' AND resolved_at IS NULL;
-- #45 boundary: shared-workspace incidents remain excluded. The single
-- identity-free installation aggregate is eligible on fire only, with a
-- kind-only summary (never source summary/details/subject or workspace).
CREATE OR REPLACE FUNCTION alert_webhook_summary(text) RETURNS text LANGUAGE sql IMMUTABLE STRICT SET search_path = pg_catalog, public, pg_temp AS $$
 SELECT CASE $1
  WHEN 'budget_threshold' THEN 'A shared workspace budget threshold was reached'
  WHEN 'spend_threshold' THEN 'An installation spend threshold was reached'
  WHEN 'spend_spike' THEN 'A spend spike was detected'
  WHEN 'error_rate' THEN 'An error rate threshold was reached'
  WHEN 'provider_failing' THEN 'An upstream connection is failing'
  WHEN 'batch_failed' THEN 'A batch failed'
  WHEN 'batch_stalled' THEN 'A batch stalled'
  WHEN 'scim_last_admin' THEN 'SCIM tried to remove the last Platform Admin'
  WHEN 'partitions_missing' THEN 'Future history partitions are missing'
  WHEN 'history_orphans' THEN 'History parent verification found orphan rows'
  WHEN 'aged_unknown_hold' THEN 'Holds with unknown cost need review'
  ELSE NULL
 END
$$;
CREATE OR REPLACE FUNCTION alert_webhook_payload(p_delivery uuid, p_event uuid, p_transition text) RETURNS text LANGUAGE plpgsql STABLE SET search_path = pg_catalog, public, pg_temp AS $$
DECLARE e public.alert_events; installation_id uuid; safe_summary text;
BEGIN
 IF p_delivery IS NULL THEN RETURN NULL; END IF;
 SELECT id INTO installation_id FROM public.installation WHERE singleton;
 IF installation_id IS NULL THEN RETURN NULL; END IF;
 IF p_event IS NULL THEN
  IF p_transition IS NOT NULL THEN RETURN NULL; END IF;
  RETURN jsonb_build_object('schema','omg.alert.v1','installation_id',installation_id,'delivery_id',p_delivery,'test',true,'event',NULL)::text;
 END IF;
 IF p_transition IS NULL OR p_transition NOT IN ('fired','resolved') THEN RETURN NULL; END IF;
 SELECT * INTO e FROM public.alert_events WHERE id=p_event;
 IF NOT FOUND THEN RETURN NULL; END IF;
 IF e.kind IN ('key_expiring','admission_ceiling')
  OR (e.kind='aged_unknown_hold' AND (e.workspace_id IS NOT NULL OR p_transition<>'fired'))
  OR (p_transition='resolved' AND e.resolution IS DISTINCT FROM 'cleared')
  OR (e.workspace_id IS NOT NULL AND NOT EXISTS(SELECT FROM public.workspaces w WHERE w.id=e.workspace_id AND w.kind IN ('team','project') AND w.disabled_at IS NULL))
  OR NOT (
   (e.rule_id IS NOT NULL AND EXISTS(SELECT FROM public.alert_rules r WHERE r.id=e.rule_id AND r.scope='installation' AND r.enabled AND r.deleted_at IS NULL AND r.notify_platform_admins))
   OR (e.rule_id IS NULL AND e.workspace_id IS NULL AND e.builtin IN ('scim_last_admin','partitions_missing','history_orphans'))
   OR (e.kind='aged_unknown_hold' AND e.workspace_id IS NULL AND e.rule_id IS NULL AND e.builtin IS NULL)
  ) THEN RETURN NULL; END IF;
 safe_summary=public.alert_webhook_summary(e.kind);
 IF safe_summary IS NULL THEN RETURN NULL; END IF;
 RETURN jsonb_build_object(
  'schema','omg.alert.v1','installation_id',installation_id,'delivery_id',p_delivery,'test',false,
  'event',jsonb_build_object('id',e.id,'transition',p_transition,'kind',e.kind,'severity',e.severity,
   'occurred_at',to_char((CASE WHEN p_transition='resolved' THEN e.resolved_at ELSE e.fired_at END) AT TIME ZONE 'UTC','YYYY-MM-DD"T"HH24:MI:SS.US"Z"'),
   'summary',safe_summary))::text;
END $$;
RESET lock_timeout;
