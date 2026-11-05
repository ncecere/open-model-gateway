-- #84: reviewed Team/Project onboarding templates. A template has immutable
-- versions; a version is reviewed by a Platform Admin other than its author
-- before it can be published; at most one version of a template is
-- published at a time; any version can be retired. Applying a published
-- version (84b) records an immutable application. Nothing is deleted.
CREATE TABLE workspace_templates (
 id uuid PRIMARY KEY,
 name text NOT NULL UNIQUE CHECK (char_length(name) BETWEEN 1 AND 120 AND name !~ '[[:cntrl:]]'),
 created_by uuid NOT NULL REFERENCES users(id),
 created_at timestamptz NOT NULL DEFAULT clock_timestamp()
);
CREATE TABLE workspace_template_versions (
 id uuid PRIMARY KEY,
 template_id uuid NOT NULL REFERENCES workspace_templates(id),
 version integer NOT NULL CHECK (version >= 1),
 -- The reviewed content (references only) and its SHA-256, as submitted.
 content jsonb NOT NULL CHECK (jsonb_typeof(content) = 'object' AND octet_length(content::text) <= 65536),
 content_hash bytea NOT NULL CHECK (octet_length(content_hash) = 32),
 author uuid NOT NULL REFERENCES users(id),
 created_at timestamptz NOT NULL DEFAULT clock_timestamp(),
 reviewed_by uuid REFERENCES users(id),
 reviewed_at timestamptz,
 published_at timestamptz,
 retired_at timestamptz,
 UNIQUE (template_id, version),
 CHECK ((reviewed_by IS NULL) = (reviewed_at IS NULL)),
 CHECK (reviewed_by IS DISTINCT FROM author),
 CHECK (published_at IS NULL OR reviewed_at IS NOT NULL)
);
CREATE UNIQUE INDEX workspace_template_versions_published ON workspace_template_versions(template_id) WHERE published_at IS NOT NULL AND retired_at IS NULL;

-- Server clocks; versions are created pending; each transition happens once,
-- in order (review, then publication, retirement at any time); the
-- identity and content never change.
CREATE FUNCTION workspace_template_versions_guard() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 IF TG_OP = 'INSERT' THEN
  IF NEW.reviewed_by IS NOT NULL OR NEW.reviewed_at IS NOT NULL OR NEW.published_at IS NOT NULL OR NEW.retired_at IS NOT NULL THEN
   RAISE EXCEPTION 'template versions are created pending review' USING ERRCODE = '23514';
  END IF;
  NEW.created_at := clock_timestamp();
  RETURN NEW;
 END IF;
 IF (NEW.id, NEW.template_id, NEW.version, NEW.content, NEW.content_hash, NEW.author, NEW.created_at)
    IS DISTINCT FROM (OLD.id, OLD.template_id, OLD.version, OLD.content, OLD.content_hash, OLD.author, OLD.created_at)
    OR (OLD.reviewed_at IS NOT NULL AND (NEW.reviewed_by, NEW.reviewed_at) IS DISTINCT FROM (OLD.reviewed_by, OLD.reviewed_at))
    OR (OLD.published_at IS NOT NULL AND NEW.published_at IS DISTINCT FROM OLD.published_at)
    OR (OLD.retired_at IS NOT NULL AND NEW.retired_at IS DISTINCT FROM OLD.retired_at)
    OR (OLD.retired_at IS NOT NULL AND (NEW.reviewed_at IS DISTINCT FROM OLD.reviewed_at OR NEW.published_at IS DISTINCT FROM OLD.published_at)) THEN
  RAISE EXCEPTION 'template versions are immutable; each step happens once' USING ERRCODE = '23514';
 END IF;
 IF NEW.reviewed_at IS DISTINCT FROM OLD.reviewed_at THEN NEW.reviewed_at := clock_timestamp(); END IF;
 IF NEW.published_at IS DISTINCT FROM OLD.published_at THEN NEW.published_at := clock_timestamp(); END IF;
 IF NEW.retired_at IS DISTINCT FROM OLD.retired_at THEN NEW.retired_at := clock_timestamp(); END IF;
 RETURN NEW;
END $$;
CREATE TRIGGER workspace_template_versions_guard BEFORE INSERT OR UPDATE ON workspace_template_versions FOR EACH ROW EXECUTE FUNCTION workspace_template_versions_guard();
CREATE TRIGGER workspace_template_versions_no_delete BEFORE DELETE ON workspace_template_versions FOR EACH ROW EXECUTE FUNCTION immutable_history();
CREATE TRIGGER workspace_template_versions_no_truncate BEFORE TRUNCATE ON workspace_template_versions FOR EACH STATEMENT EXECUTE FUNCTION immutable_history();
CREATE TRIGGER workspace_templates_immutable BEFORE UPDATE OR DELETE ON workspace_templates FOR EACH ROW EXECUTE FUNCTION immutable_history();
CREATE TRIGGER workspace_templates_no_truncate BEFORE TRUNCATE ON workspace_templates FOR EACH STATEMENT EXECUTE FUNCTION immutable_history();
REVOKE ALL ON FUNCTION workspace_template_versions_guard() FROM PUBLIC;
