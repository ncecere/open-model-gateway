-- #86. Additive only: existing deployments start at revision zero. No invented
-- snapshots or audit rewrites. Enablement writers cannot skip revision/history.
ALTER TABLE deployments ADD COLUMN revision bigint NOT NULL DEFAULT 0 CHECK (revision >= 0);

CREATE TABLE configuration_changes (
    id uuid PRIMARY KEY,
    created_at timestamptz NOT NULL,
    audit_event_id uuid NOT NULL,
    audit_created_at timestamptz NOT NULL,
    target_kind text NOT NULL CHECK (target_kind = 'deployment'),
    deployment_id uuid NOT NULL,
    schema_version smallint NOT NULL CHECK (schema_version = 1),
    before_enabled boolean,
    after_enabled boolean NOT NULL,
    revision bigint NOT NULL CHECK (revision > 0),
    CHECK (before_enabled IS NULL OR before_enabled <> after_enabled),
    CHECK (id = audit_event_id AND created_at = audit_created_at),
    UNIQUE (deployment_id, revision),
    UNIQUE (audit_event_id, audit_created_at)
);
-- Audit identity is (id,created_at), NOT id alone. Deliberately a logical link:
-- a foreign key to partitioned audit_events would prevent its archive/detach
-- lifecycle. History retains IDs and values if the audit partition leaves public.
-- Likewise no target FK: removing a target must not remove historical evidence.
CREATE INDEX configuration_changes_target_page ON configuration_changes
    (deployment_id, created_at DESC, id DESC);
-- Legacy events without snapshots use the same bounded target/cursor access.
CREATE INDEX audit_events_deployment_history ON audit_events
    (resource_id, created_at DESC, id DESC)
    WHERE workspace_id IS NULL AND resource_type IN ('deployment','deployments')
      AND action IN ('deployment.created','configuration.updated');

-- Insert-time identity checks (not a retaining FK), following the existing
-- partition-history pattern. Personal audit events can never become platform
-- configuration history, even through a direct runtime INSERT.
CREATE FUNCTION configuration_changes_validate() RETURNS trigger
LANGUAGE plpgsql SET search_path = pg_catalog, public AS $$ BEGIN
    IF NOT EXISTS (
        SELECT FROM public.audit_events a WHERE (a.id,a.created_at)=(NEW.audit_event_id,NEW.audit_created_at)
        AND a.workspace_id IS NULL AND a.resource_id=NEW.deployment_id
        AND ((NEW.before_enabled IS NULL AND a.action='deployment.created' AND a.resource_type='deployment')
          OR (NEW.before_enabled IS NOT NULL AND a.action='configuration.updated' AND a.resource_type='deployments'))
    ) OR NOT EXISTS (
        SELECT FROM public.deployments d WHERE d.id=NEW.deployment_id
        AND d.revision=NEW.revision AND d.enabled=NEW.after_enabled
    ) THEN
        RAISE EXCEPTION 'configuration history identity does not match its audit and deployment' USING ERRCODE='23503';
    END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER configuration_changes_validate BEFORE INSERT ON configuration_changes
    FOR EACH ROW EXECUTE FUNCTION configuration_changes_validate();

CREATE FUNCTION configuration_changes_immutable() RETURNS trigger
LANGUAGE plpgsql AS $$ BEGIN
    RAISE EXCEPTION 'configuration history is immutable' USING ERRCODE = '23514';
END $$;
CREATE TRIGGER configuration_changes_no_update BEFORE UPDATE OR DELETE ON configuration_changes
    FOR EACH ROW EXECUTE FUNCTION configuration_changes_immutable();
CREATE TRIGGER configuration_changes_no_truncate BEFORE TRUNCATE ON configuration_changes
    FOR EACH STATEMENT EXECUTE FUNCTION configuration_changes_immutable();

CREATE FUNCTION deployment_configuration_revision() RETURNS trigger
LANGUAGE plpgsql AS $$ BEGIN
    IF TG_OP = 'INSERT' THEN
        IF NEW.revision <> 0 THEN
            RAISE EXCEPTION 'deployment revision is database maintained' USING ERRCODE = '23514';
        END IF;
        NEW.revision := 1;
    ELSE
        IF NEW.revision <> OLD.revision THEN
            RAISE EXCEPTION 'deployment revision is database maintained' USING ERRCODE = '23514';
        END IF;
        IF NEW.enabled IS DISTINCT FROM OLD.enabled THEN
            NEW.revision := OLD.revision + 1;
        END IF;
    END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER deployment_configuration_revision BEFORE INSERT OR UPDATE ON deployments
    FOR EACH ROW EXECUTE FUNCTION deployment_configuration_revision();

-- Invoker rights: only SELECT/INSERT on history/audit is needed. Application
-- deployment operations set transaction-local actor/token IDs after live authz.
-- Non-management writers (bootstrap/demo/SQL) are explicitly system-authored;
-- context is never inferred from a previous transaction or a resource owner.
CREATE FUNCTION deployment_configuration_capture() RETURNS trigger
LANGUAGE plpgsql SET search_path = pg_catalog, public AS $$
DECLARE
    event_id uuid := gen_random_uuid();
    event_time timestamptz := clock_timestamp();
    before_value boolean;
    action_code text;
    resource_code text;
BEGIN
    IF TG_OP = 'UPDATE' AND NEW.enabled IS NOT DISTINCT FROM OLD.enabled THEN
        RETURN NEW;
    END IF;
    IF TG_OP = 'INSERT' THEN
        action_code := 'deployment.created';
        resource_code := 'deployment';
    ELSE
        before_value := OLD.enabled;
        action_code := 'configuration.updated';
        resource_code := 'deployments';
    END IF;
    INSERT INTO public.audit_events(id,created_at,actor_user_id,admin_token_id,action,resource_type,resource_id,metadata)
    VALUES(event_id,event_time,
        nullif(current_setting('omg.configuration_actor',true),'')::uuid,
        nullif(current_setting('omg.configuration_token',true),'')::uuid,
        action_code,resource_code,NEW.id,
        CASE WHEN TG_OP='INSERT' THEN '{}'::jsonb ELSE jsonb_build_object('enabled',NEW.enabled) END);
    INSERT INTO public.configuration_changes(id,created_at,audit_event_id,audit_created_at,target_kind,deployment_id,schema_version,before_enabled,after_enabled,revision)
    VALUES(event_id,event_time,event_id,event_time,'deployment',NEW.id,1,before_value,NEW.enabled,NEW.revision);
    RETURN NEW;
END $$;
CREATE TRIGGER deployment_configuration_capture AFTER INSERT OR UPDATE ON deployments
    FOR EACH ROW EXECUTE FUNCTION deployment_configuration_capture();

REVOKE ALL ON FUNCTION configuration_changes_validate(),configuration_changes_immutable(),
    deployment_configuration_revision(),deployment_configuration_capture() FROM PUBLIC;
