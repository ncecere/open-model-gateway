-- #88: explicit-target bulk deployment disablement. One operation per actor
-- and idempotency key; each item records its outcome exactly once, in the same
-- transaction as its mutation. The request applying the operation holds a
-- lease; once every item is decided, or the lease has lapsed (the request was
-- interrupted), the operation completes exactly once: items still undecided
-- are recorded unattempted (never applied), with one outcome audit event.
CREATE TABLE configuration_bulk_operations (
    id uuid PRIMARY KEY,
    actor_user_id uuid NOT NULL REFERENCES users(id),
    idempotency_key text NOT NULL CHECK (idempotency_key ~ '^[A-Za-z0-9._:-]{1,128}$'),
    payload_digest bytea NOT NULL CHECK (octet_length(payload_digest) = 32),
    action text NOT NULL CHECK (action = 'deployment.disable'),
    item_count smallint NOT NULL CHECK (item_count BETWEEN 1 AND 50),
    created_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    lease_until timestamptz NOT NULL DEFAULT clock_timestamp() + interval '30 seconds',
    completed_at timestamptz,
    UNIQUE (actor_user_id, idempotency_key),
    CHECK (completed_at IS NULL OR completed_at >= created_at)
);
CREATE TABLE configuration_bulk_items (
    operation_id uuid NOT NULL REFERENCES configuration_bulk_operations(id),
    position smallint NOT NULL CHECK (position BETWEEN 0 AND 49),
    deployment_id uuid NOT NULL,
    expected_revision bigint NOT NULL CHECK (expected_revision >= 0),
    outcome text CHECK (outcome IN ('committed','unchanged','refused','conflict','unattempted')),
    reason text CHECK (reason IN ('not_found','forbidden','revision_conflict')),
    change_id uuid,
    revision bigint CHECK (revision >= 0),
    decided_at timestamptz,
    PRIMARY KEY (operation_id, position),
    UNIQUE (operation_id, deployment_id),
    CHECK ((outcome IS NULL) = (decided_at IS NULL)),
    CHECK ((outcome = 'committed') = (change_id IS NOT NULL)),
    CHECK ((outcome IN ('refused','conflict')) = (reason IS NOT NULL)),
    CHECK (outcome IS NOT NULL OR (revision IS NULL AND reason IS NULL AND change_id IS NULL)),
    CHECK (outcome IS DISTINCT FROM 'unattempted' OR revision IS NULL),
    CHECK ((outcome IN ('committed','unchanged','conflict')) = (revision IS NOT NULL)),
    CHECK (reason IS NULL OR (outcome = 'conflict') = (reason = 'revision_conflict'))
);
-- No target FK, like configuration history: removing a target keeps evidence.

-- Items are created undecided with their operation, within its declared count.
CREATE FUNCTION configuration_bulk_items_insert() RETURNS trigger
LANGUAGE plpgsql SET search_path = pg_catalog, public AS $$ BEGIN
    IF NEW.outcome IS NOT NULL OR NEW.reason IS NOT NULL OR NEW.change_id IS NOT NULL
       OR NEW.revision IS NOT NULL OR NEW.decided_at IS NOT NULL
       OR NOT EXISTS (SELECT FROM public.configuration_bulk_operations o WHERE o.id = NEW.operation_id
                      AND o.completed_at IS NULL AND NEW.position < o.item_count) THEN
        RAISE EXCEPTION 'bulk items are created undecided, within their operation' USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER configuration_bulk_items_insert BEFORE INSERT ON configuration_bulk_items
    FOR EACH ROW EXECUTE FUNCTION configuration_bulk_items_insert();
CREATE FUNCTION configuration_bulk_operations_insert() RETURNS trigger
LANGUAGE plpgsql AS $$ BEGIN
    IF NEW.completed_at IS NOT NULL THEN
        RAISE EXCEPTION 'bulk operations are created incomplete' USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER configuration_bulk_operations_insert BEFORE INSERT ON configuration_bulk_operations
    FOR EACH ROW EXECUTE FUNCTION configuration_bulk_operations_insert();

-- Outcomes are written once, before completion; a committed outcome names the
-- change this operation made to this target at the recorded revision.
CREATE FUNCTION configuration_bulk_items_decide_once() RETURNS trigger
LANGUAGE plpgsql SET search_path = pg_catalog, public AS $$ BEGIN
    IF OLD.outcome IS NOT NULL
       OR (NEW.operation_id, NEW.position, NEW.deployment_id, NEW.expected_revision)
          IS DISTINCT FROM (OLD.operation_id, OLD.position, OLD.deployment_id, OLD.expected_revision)
       OR NEW.outcome IS NULL
       OR NOT EXISTS (SELECT FROM public.configuration_bulk_operations o WHERE o.id = NEW.operation_id AND o.completed_at IS NULL)
       OR (NEW.outcome = 'committed' AND NOT EXISTS (
            SELECT FROM public.configuration_changes c WHERE c.id = NEW.change_id
            AND c.bulk_operation_id = NEW.operation_id AND c.deployment_id = NEW.deployment_id
            AND c.revision = NEW.revision AND c.revision = NEW.expected_revision + 1)) THEN
        RAISE EXCEPTION 'bulk item outcomes are recorded once' USING ERRCODE = '23514';
    END IF;
    NEW.decided_at := clock_timestamp(); -- the server's clock, never the caller's
    RETURN NEW;
END $$;
CREATE TRIGGER configuration_bulk_items_decide_once BEFORE UPDATE ON configuration_bulk_items
    FOR EACH ROW EXECUTE FUNCTION configuration_bulk_items_decide_once();
-- Only the lease moves while incomplete; completion happens once, after every
-- declared item is decided; a completed operation never changes.
CREATE FUNCTION configuration_bulk_operations_complete_once() RETURNS trigger
LANGUAGE plpgsql SET search_path = pg_catalog, public AS $$ BEGIN
    IF OLD.completed_at IS NOT NULL
       OR (NEW.id, NEW.actor_user_id, NEW.idempotency_key, NEW.payload_digest, NEW.action, NEW.item_count, NEW.created_at)
          IS DISTINCT FROM (OLD.id, OLD.actor_user_id, OLD.idempotency_key, OLD.payload_digest, OLD.action, OLD.item_count, OLD.created_at)
       OR (NEW.completed_at IS NOT NULL AND (
            NEW.lease_until IS DISTINCT FROM OLD.lease_until
            OR EXISTS (SELECT FROM public.configuration_bulk_items i WHERE i.operation_id = NEW.id AND i.outcome IS NULL)
            OR (SELECT count(*) FROM public.configuration_bulk_items i WHERE i.operation_id = NEW.id) <> NEW.item_count)) THEN
        RAISE EXCEPTION 'bulk operations complete once, after every item' USING ERRCODE = '23514';
    END IF;
    -- The server's clock; a lease is never set more than a minute ahead.
    IF NEW.completed_at IS NOT NULL THEN
        NEW.completed_at := clock_timestamp();
    ELSIF NEW.lease_until > clock_timestamp() + interval '1 minute' THEN
        RAISE EXCEPTION 'bulk leases are short' USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER configuration_bulk_operations_complete_once BEFORE UPDATE ON configuration_bulk_operations
    FOR EACH ROW EXECUTE FUNCTION configuration_bulk_operations_complete_once();
CREATE FUNCTION configuration_bulk_immutable() RETURNS trigger
LANGUAGE plpgsql AS $$ BEGIN
    RAISE EXCEPTION 'bulk operation records are immutable' USING ERRCODE = '23514';
END $$;
CREATE TRIGGER configuration_bulk_operations_no_delete BEFORE DELETE ON configuration_bulk_operations
    FOR EACH ROW EXECUTE FUNCTION configuration_bulk_immutable();
CREATE TRIGGER configuration_bulk_operations_no_truncate BEFORE TRUNCATE ON configuration_bulk_operations
    FOR EACH STATEMENT EXECUTE FUNCTION configuration_bulk_immutable();
CREATE TRIGGER configuration_bulk_items_no_delete BEFORE DELETE ON configuration_bulk_items
    FOR EACH ROW EXECUTE FUNCTION configuration_bulk_immutable();
CREATE TRIGGER configuration_bulk_items_no_truncate BEFORE TRUNCATE ON configuration_bulk_items
    FOR EACH STATEMENT EXECUTE FUNCTION configuration_bulk_immutable();

-- History correlation: a change made by a bulk item names its operation.
ALTER TABLE configuration_changes ADD COLUMN bulk_operation_id uuid;

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
    ))) OR (NEW.bulk_operation_id IS NOT NULL AND (NEW.before_enabled IS DISTINCT FROM true OR NEW.after_enabled OR NOT EXISTS (
        -- A pending item of an incomplete operation, for this target at the
        -- next revision, recorded by the operation's own actor (browser only).
        SELECT FROM public.configuration_bulk_items i
        JOIN public.configuration_bulk_operations o ON o.id=i.operation_id
        JOIN public.audit_events a ON (a.id,a.created_at)=(NEW.audit_event_id,NEW.audit_created_at)
        WHERE i.operation_id=NEW.bulk_operation_id AND i.deployment_id=NEW.deployment_id
        AND i.outcome IS NULL AND NEW.revision=i.expected_revision+1 AND o.completed_at IS NULL
        AND a.actor_user_id=o.actor_user_id AND a.admin_token_id IS NULL
    ))) THEN
        RAISE EXCEPTION 'configuration history identity does not match its audit and deployment' USING ERRCODE='23503';
    END IF;
    RETURN NEW;
END $$;

-- The bulk route sets omg.configuration_bulk (transaction-local) only for the
-- item it is applying; like the restore link it is consumed by one change.
CREATE OR REPLACE FUNCTION deployment_configuration_capture() RETURNS trigger
LANGUAGE plpgsql SET search_path = pg_catalog, public AS $$
DECLARE
    event_id uuid := gen_random_uuid();
    event_time timestamptz := clock_timestamp();
    before_value boolean;
    action_code text;
    resource_code text;
    restore uuid;
    bulk uuid;
    meta jsonb;
BEGIN
    IF TG_OP = 'UPDATE' AND NEW.enabled IS NOT DISTINCT FROM OLD.enabled THEN
        RETURN NEW;
    END IF;
    IF TG_OP = 'INSERT' THEN
        action_code := 'deployment.created';
        resource_code := 'deployment';
        meta := '{}'::jsonb;
    ELSE
        before_value := OLD.enabled;
        action_code := 'configuration.updated';
        resource_code := 'deployments';
        restore := nullif(current_setting('omg.configuration_restore',true),'')::uuid;
        bulk := nullif(current_setting('omg.configuration_bulk',true),'')::uuid;
        -- Consumed by this change: a later change in the same transaction never
        -- inherits either link.
        PERFORM set_config('omg.configuration_restore','',true);
        PERFORM set_config('omg.configuration_bulk','',true);
        meta := jsonb_build_object('enabled',NEW.enabled);
        IF restore IS NOT NULL THEN meta := meta || jsonb_build_object('restored_from',restore); END IF;
        IF bulk IS NOT NULL THEN meta := meta || jsonb_build_object('bulk_operation_id',bulk); END IF;
    END IF;
    INSERT INTO public.audit_events(id,created_at,actor_user_id,admin_token_id,action,resource_type,resource_id,metadata)
    VALUES(event_id,event_time,
        nullif(current_setting('omg.configuration_actor',true),'')::uuid,
        nullif(current_setting('omg.configuration_token',true),'')::uuid,
        action_code,resource_code,NEW.id,meta);
    INSERT INTO public.configuration_changes(id,created_at,audit_event_id,audit_created_at,target_kind,deployment_id,schema_version,before_enabled,after_enabled,revision,restored_from,bulk_operation_id)
    VALUES(event_id,event_time,event_id,event_time,'deployment',NEW.id,1,before_value,NEW.enabled,NEW.revision,restore,bulk);
    RETURN NEW;
END $$;

-- The other direction, at commit: a change linked to an operation is that
-- operation's committed outcome for its target, so history never names an
-- operation that recorded something else (or nothing) for it.
CREATE FUNCTION configuration_changes_bulk_committed() RETURNS trigger
LANGUAGE plpgsql SET search_path = pg_catalog, public AS $$ BEGIN
    IF NOT EXISTS (SELECT FROM public.configuration_bulk_items i WHERE i.operation_id = NEW.bulk_operation_id
                   AND i.deployment_id = NEW.deployment_id AND i.outcome = 'committed' AND i.change_id = NEW.id) THEN
        RAISE EXCEPTION 'a bulk-linked change must be its operation''s committed outcome' USING ERRCODE = '23514';
    END IF;
    RETURN NULL;
END $$;
CREATE CONSTRAINT TRIGGER configuration_changes_bulk_committed AFTER INSERT ON configuration_changes
    DEFERRABLE INITIALLY DEFERRED FOR EACH ROW WHEN (NEW.bulk_operation_id IS NOT NULL)
    EXECUTE FUNCTION configuration_changes_bulk_committed();

REVOKE ALL ON FUNCTION configuration_changes_bulk_committed(),configuration_bulk_items_decide_once(),configuration_bulk_operations_complete_once(),
    configuration_bulk_immutable(),configuration_bulk_items_insert(),configuration_bulk_operations_insert() FROM PUBLIC;
