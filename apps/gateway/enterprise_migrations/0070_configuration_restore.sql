-- #87: a restore is a new deployment change linked to the historical change
-- whose configuration it restores. Additive: existing rows keep NULL. The link
-- is logical (like the audit link) and immutable with the rest of the row.
ALTER TABLE configuration_changes ADD COLUMN restored_from uuid;

-- The link must name an earlier change of the same deployment whose recorded
-- value is the value now applied. Unchanged otherwise from 0067.
CREATE OR REPLACE FUNCTION configuration_changes_validate() RETURNS trigger
LANGUAGE plpgsql SET search_path = pg_catalog, public AS $$ BEGIN
    IF NOT EXISTS (
        SELECT FROM public.audit_events a WHERE (a.id,a.created_at)=(NEW.audit_event_id,NEW.audit_created_at)
        AND a.workspace_id IS NULL AND a.resource_id=NEW.deployment_id
        AND ((NEW.before_enabled IS NULL AND a.action='deployment.created' AND a.resource_type='deployment')
          OR (NEW.before_enabled IS NOT NULL AND a.action='configuration.updated' AND a.resource_type='deployments'))
    ) OR NOT EXISTS (
        SELECT FROM public.deployments d WHERE d.id=NEW.deployment_id
        AND d.revision=NEW.revision AND d.enabled=NEW.after_enabled
    ) OR (NEW.restored_from IS NOT NULL AND (NEW.before_enabled IS NULL OR NOT EXISTS (
        SELECT FROM public.configuration_changes s WHERE s.id=NEW.restored_from
        AND s.deployment_id=NEW.deployment_id AND s.after_enabled=NEW.after_enabled
        AND s.revision<NEW.revision
    ))) THEN
        RAISE EXCEPTION 'configuration history identity does not match its audit and deployment' USING ERRCODE='23503';
    END IF;
    RETURN NEW;
END $$;

-- The application sets omg.configuration_restore (transaction-local) only for
-- an authorized restore, after its checks. Creation never carries a link.
CREATE OR REPLACE FUNCTION deployment_configuration_capture() RETURNS trigger
LANGUAGE plpgsql SET search_path = pg_catalog, public AS $$
DECLARE
    event_id uuid := gen_random_uuid();
    event_time timestamptz := clock_timestamp();
    before_value boolean;
    action_code text;
    resource_code text;
    restore uuid;
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
        restore := nullif(current_setting('omg.configuration_restore',true),'')::uuid;
    END IF;
    INSERT INTO public.audit_events(id,created_at,actor_user_id,admin_token_id,action,resource_type,resource_id,metadata)
    VALUES(event_id,event_time,
        nullif(current_setting('omg.configuration_actor',true),'')::uuid,
        nullif(current_setting('omg.configuration_token',true),'')::uuid,
        action_code,resource_code,NEW.id,
        CASE WHEN TG_OP='INSERT' THEN '{}'::jsonb
             WHEN restore IS NULL THEN jsonb_build_object('enabled',NEW.enabled)
             ELSE jsonb_build_object('enabled',NEW.enabled,'restored_from',restore) END);
    INSERT INTO public.configuration_changes(id,created_at,audit_event_id,audit_created_at,target_kind,deployment_id,schema_version,before_enabled,after_enabled,revision,restored_from)
    VALUES(event_id,event_time,event_id,event_time,'deployment',NEW.id,1,before_value,NEW.enabled,NEW.revision,restore);
    RETURN NEW;
END $$;
