-- #84 slice B: applying a published template version creates one workspace;
-- the application is recorded immutably with the version and its hash, per
-- applying actor and idempotency key. A later publication never reapplies.
CREATE TABLE workspace_template_applications (
 id uuid PRIMARY KEY,
 template_id uuid NOT NULL REFERENCES workspace_templates(id),
 version integer NOT NULL,
 content_hash bytea NOT NULL CHECK (octet_length(content_hash) = 32),
 workspace_id uuid NOT NULL UNIQUE REFERENCES workspaces(id),
 applied_by uuid NOT NULL REFERENCES users(id),
 idempotency_digest bytea NOT NULL CHECK (octet_length(idempotency_digest) = 32),
 payload_digest bytea NOT NULL CHECK (octet_length(payload_digest) = 32),
 created_at timestamptz NOT NULL DEFAULT clock_timestamp(),
 UNIQUE (applied_by, idempotency_digest),
 FOREIGN KEY (template_id, version) REFERENCES workspace_template_versions(template_id, version)
);
-- Only a published, unretired version is applied, with its own hash, by a
-- current Platform Admin, to a shared workspace of the template's kind created
-- in this same transaction (so an existing workspace cannot be claimed); the
-- server sets the clock.
CREATE FUNCTION workspace_template_applications_guard() RETURNS trigger LANGUAGE plpgsql SET search_path = pg_catalog, public AS $$
BEGIN
 IF NOT EXISTS (SELECT FROM public.workspace_template_versions v JOIN public.workspaces w ON w.id = NEW.workspace_id
                WHERE v.template_id = NEW.template_id AND v.version = NEW.version
                AND v.published_at IS NOT NULL AND v.retired_at IS NULL AND v.content_hash = NEW.content_hash
                AND w.kind IN ('team','project') AND w.kind = v.content->>'kind'
                AND w.xmin::text = pg_current_xact_id()::xid::text)
    OR NOT EXISTS (SELECT FROM public.effective_platform_roles p WHERE p.user_id = NEW.applied_by AND p.role = 'admin') THEN
  RAISE EXCEPTION 'a template application names a published version and a shared workspace' USING ERRCODE = '23514';
 END IF;
 NEW.created_at := clock_timestamp();
 RETURN NEW;
END $$;
CREATE TRIGGER workspace_template_applications_guard BEFORE INSERT ON workspace_template_applications FOR EACH ROW EXECUTE FUNCTION workspace_template_applications_guard();
CREATE TRIGGER workspace_template_applications_immutable BEFORE UPDATE OR DELETE ON workspace_template_applications FOR EACH ROW EXECUTE FUNCTION immutable_history();
CREATE TRIGGER workspace_template_applications_no_truncate BEFORE TRUNCATE ON workspace_template_applications FOR EACH STATEMENT EXECUTE FUNCTION immutable_history();
REVOKE ALL ON FUNCTION workspace_template_applications_guard() FROM PUBLIC;
