-- Alerts: rules, incidents (alert events), email delivery records and per-user
-- notification read state. Explicit operator upgrade only (`migrate`); never
-- applied implicitly by serve. See docs/alerts.md.
--
-- Rules are installation-wide (Platform Admins) or belong to one Team/Project
-- (its admins). Personal workspaces have built-in budget alerts only (no rows
-- here; events carry builtin='personal_budget'). Rules are soft-deleted so
-- their history stays readable.
CREATE TABLE alert_rules(
 id uuid PRIMARY KEY,
 scope text NOT NULL CHECK(scope IN ('installation','workspace')),
 workspace_id uuid REFERENCES workspaces(id),
 kind text NOT NULL CHECK(kind IN ('budget_threshold','spend_spike','error_rate','provider_failing')),
 name text NOT NULL CHECK(char_length(name) BETWEEN 1 AND 120 AND name !~ '[[:cntrl:]]'),
 enabled boolean NOT NULL DEFAULT true,
 -- budget_threshold: stacked-budget layers watched and percentage thresholds.
 budget_layers text[] CHECK(budget_layers IS NULL OR (cardinality(budget_layers) BETWEEN 1 AND 5 AND budget_layers <@ ARRAY['installation','type','override','local','key']::text[])),
 thresholds integer[] CHECK(thresholds IS NULL OR (cardinality(thresholds) BETWEEN 1 AND 5 AND 1 <= ALL(thresholds) AND 100 >= ALL(thresholds))),
 -- spend_spike: last hour >= trailing 7-day hourly average x factor/100, and >= a floor.
 spike_factor_percent integer CHECK(spike_factor_percent BETWEEN 110 AND 100000),
 min_spend_microusd bigint CHECK(min_spend_microusd >= 1),
 -- error_rate / provider_failing: attempts started in the window.
 window_minutes integer CHECK(window_minutes BETWEEN 5 AND 1440),
 error_rate_percent integer CHECK(error_rate_percent BETWEEN 1 AND 100),
 min_requests integer CHECK(min_requests BETWEEN 1 AND 100000),
 consecutive_failures integer CHECK(consecutive_failures BETWEEN 1 AND 100),
 provider_connection_id uuid REFERENCES provider_connections(id),
 -- Email recipients. In-app visibility follows live authority, not this list.
 notify_workspace_admins boolean NOT NULL DEFAULT false,
 notify_platform_admins boolean NOT NULL DEFAULT false,
 notify_emails text[] NOT NULL DEFAULT '{}' CHECK(cardinality(notify_emails) <= 10 AND array_position(notify_emails,NULL) IS NULL),
 created_by uuid REFERENCES users(id),
 created_at timestamptz NOT NULL DEFAULT now(),
 updated_by uuid REFERENCES users(id),
 updated_at timestamptz NOT NULL DEFAULT now(),
 deleted_at timestamptz,
 CHECK((scope='installation') = (workspace_id IS NULL)),
 -- Workspace rules never watch connections or the installation budget.
 CHECK(scope='installation' OR (kind<>'provider_failing' AND NOT coalesce('installation'=ANY(budget_layers),false))),
 CHECK((kind='budget_threshold') = (budget_layers IS NOT NULL AND thresholds IS NOT NULL)),
 CHECK((kind='spend_spike') = (spike_factor_percent IS NOT NULL AND min_spend_microusd IS NOT NULL)),
 CHECK((kind IN ('error_rate','provider_failing')) = (window_minutes IS NOT NULL)),
 CHECK(kind<>'error_rate' OR (error_rate_percent IS NOT NULL AND min_requests IS NOT NULL AND consecutive_failures IS NULL)),
 CHECK(kind<>'provider_failing' OR ((consecutive_failures IS NOT NULL OR error_rate_percent IS NOT NULL) AND (error_rate_percent IS NULL) = (min_requests IS NULL))),
 CHECK(kind IN ('error_rate','provider_failing') OR (error_rate_percent IS NULL AND min_requests IS NULL AND consecutive_failures IS NULL)),
 CHECK(kind='provider_failing' OR provider_connection_id IS NULL)
);
CREATE INDEX alert_rules_workspace ON alert_rules(workspace_id) WHERE workspace_id IS NOT NULL;
CREATE TRIGGER alert_rule_shared BEFORE INSERT OR UPDATE ON alert_rules FOR EACH ROW WHEN (NEW.workspace_id IS NOT NULL) EXECUTE FUNCTION require_shared_workspace();

-- One row per incident: inserted when a condition starts firing, resolved once.
-- At most one open incident per rule (or built-in) and subject, so repeated
-- evaluation never fires twice. Summaries/details are server-generated,
-- typed facts (no prompt data, no owner identity, no key names).
CREATE TABLE alert_events(
 id uuid PRIMARY KEY,
 rule_id uuid REFERENCES alert_rules(id),
 builtin text CHECK(builtin IN ('personal_budget')),
 kind text NOT NULL CHECK(kind IN ('budget_threshold','spend_spike','error_rate','provider_failing')),
 subject_key text NOT NULL CHECK(char_length(subject_key) BETWEEN 1 AND 200),
 level integer NOT NULL CHECK(level BETWEEN 1 AND 100),
 severity text NOT NULL CHECK(severity IN ('warning','critical')),
 workspace_id uuid REFERENCES workspaces(id),
 provider_connection_id uuid REFERENCES provider_connections(id),
 summary text NOT NULL CHECK(char_length(summary) BETWEEN 1 AND 200),
 details jsonb NOT NULL DEFAULT '{}' CHECK(jsonb_typeof(details)='object'),
 fired_at timestamptz NOT NULL DEFAULT clock_timestamp(),
 resolved_at timestamptz,
 resolution text CHECK(resolution IN ('cleared','superseded','rule_disabled')),
 CHECK((rule_id IS NULL) <> (builtin IS NULL)),
 CHECK(builtin IS NULL OR (kind='budget_threshold' AND workspace_id IS NOT NULL)),
 CHECK((resolved_at IS NULL) = (resolution IS NULL))
);
CREATE UNIQUE INDEX alert_events_open ON alert_events(coalesce(rule_id,'00000000-0000-0000-0000-000000000000'::uuid),coalesce(builtin,''),subject_key) WHERE resolved_at IS NULL;
CREATE INDEX alert_events_rule_time ON alert_events(rule_id,fired_at DESC) WHERE rule_id IS NOT NULL;
CREATE INDEX alert_events_workspace_time ON alert_events(workspace_id,fired_at DESC) WHERE workspace_id IS NOT NULL;
CREATE INDEX alert_events_time ON alert_events(fired_at DESC,id);
-- Resolution is set once; incidents are never deleted or rewritten.
CREATE FUNCTION alert_event_resolve_once() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 IF OLD.resolved_at IS NOT NULL OR NEW.resolved_at IS NULL OR NEW.id<>OLD.id OR NEW.rule_id IS DISTINCT FROM OLD.rule_id
  OR NEW.builtin IS DISTINCT FROM OLD.builtin OR NEW.subject_key<>OLD.subject_key OR NEW.level<>OLD.level
  OR NEW.summary<>OLD.summary OR NEW.details<>OLD.details OR NEW.fired_at<>OLD.fired_at THEN
  RAISE EXCEPTION 'alert events resolve once and are otherwise immutable';
 END IF;
 RETURN NEW;
END $$;
CREATE TRIGGER alert_events_resolve_once BEFORE UPDATE ON alert_events FOR EACH ROW EXECUTE FUNCTION alert_event_resolve_once();
CREATE TRIGGER alert_events_no_delete BEFORE DELETE ON alert_events FOR EACH ROW EXECUTE FUNCTION immutable_history();
CREATE TRIGGER alert_events_no_truncate BEFORE TRUNCATE ON alert_events FOR EACH STATEMENT EXECUTE FUNCTION immutable_history();

-- Email delivery per incident transition. Counts and a failure category only:
-- never addresses, bodies or relay replies.
CREATE TABLE alert_deliveries(
 id uuid PRIMARY KEY,
 event_id uuid NOT NULL REFERENCES alert_events(id),
 transition text NOT NULL CHECK(transition IN ('fired','resolved')),
 status text NOT NULL DEFAULT 'pending' CHECK(status IN ('pending','sent','partial','failed','not_configured','no_recipients')),
 recipients integer NOT NULL DEFAULT 0 CHECK(recipients BETWEEN 0 AND 100),
 sent integer NOT NULL DEFAULT 0 CHECK(sent >= 0),
 failed integer NOT NULL DEFAULT 0 CHECK(failed >= 0),
 error text CHECK(error IN ('credential','address','connection','tls','authentication','rejected','timeout','interrupted')),
 created_at timestamptz NOT NULL DEFAULT clock_timestamp(),
 completed_at timestamptz,
 UNIQUE(event_id,transition),
 CHECK((status='pending') = (completed_at IS NULL))
);
CREATE INDEX alert_deliveries_pending ON alert_deliveries(created_at) WHERE status='pending';
CREATE TRIGGER alert_deliveries_no_delete BEFORE DELETE ON alert_deliveries FOR EACH ROW EXECUTE FUNCTION immutable_history();
CREATE TRIGGER alert_deliveries_no_truncate BEFORE TRUNCATE ON alert_deliveries FOR EACH STATEMENT EXECUTE FUNCTION immutable_history();

-- Per-user read state of in-app notifications (insert only).
CREATE TABLE alert_reads(
 user_id uuid NOT NULL REFERENCES users(id),
 event_id uuid NOT NULL REFERENCES alert_events(id),
 read_at timestamptz NOT NULL DEFAULT now(),
 PRIMARY KEY(user_id,event_id)
);

-- Budget alerts sum one workspace's reservations per budget window.
CREATE INDEX governance_workspace_admitted ON governance_reservations(workspace_id,admitted_at);
