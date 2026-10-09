-- File store (0019). Explicit operator upgrade only (`migrate`); never applied
-- implicitly by serve. See docs/file-storage.md.
--
-- Objects live in the configured store (local disk or S3-compatible), always
-- encrypted by the gateway. This table holds metadata only: never contents.
-- Rows are never deleted: removing an object sets deleted_at and clears the
-- filename/content type. Identity and ownership columns are fixed at insert
-- (runtime has no UPDATE on them); committed fields are written once and a
-- deleted row never changes again (trigger).

-- Per purpose group: an enable toggle for groups that hold customer content
-- (batch inputs/outputs, video outputs, user files) and a retention window.
-- Exports and branding follow the backend being configured; branding never
-- expires. Health-check results are recorded with a fingerprint of the store
-- configuration they apply to.
ALTER TABLE installation_settings
 ADD COLUMN file_batch_enabled boolean NOT NULL DEFAULT false,
 ADD COLUMN file_batch_retention_days integer NOT NULL DEFAULT 7 CHECK(file_batch_retention_days BETWEEN 1 AND 365),
 ADD COLUMN file_video_enabled boolean NOT NULL DEFAULT false,
 ADD COLUMN file_video_retention_days integer NOT NULL DEFAULT 7 CHECK(file_video_retention_days BETWEEN 1 AND 365),
 ADD COLUMN file_user_files_enabled boolean NOT NULL DEFAULT false,
 ADD COLUMN file_user_files_retention_days integer NOT NULL DEFAULT 30 CHECK(file_user_files_retention_days BETWEEN 1 AND 365),
 ADD COLUMN file_export_retention_days integer NOT NULL DEFAULT 1 CHECK(file_export_retention_days BETWEEN 1 AND 365),
 ADD COLUMN file_store_last_check_at timestamptz,
 ADD COLUMN file_store_last_check_ok boolean,
 ADD COLUMN file_store_last_check_error text CHECK(file_store_last_check_error IS NULL OR file_store_last_check_error ~ '^[a-z_]{1,32}$'),
 ADD COLUMN file_store_last_check_target text CHECK(file_store_last_check_target IS NULL OR file_store_last_check_target ~ '^[0-9a-f]{64}$'),
 ADD CONSTRAINT installation_settings_file_check_complete CHECK(
  (file_store_last_check_at IS NULL) = (file_store_last_check_ok IS NULL)
  AND (file_store_last_check_at IS NULL) = (file_store_last_check_target IS NULL)
  AND (file_store_last_check_error IS NULL OR file_store_last_check_ok = false));

CREATE TABLE stored_files(
 id uuid PRIMARY KEY,
 object_key text NOT NULL UNIQUE,
 purpose text NOT NULL CHECK(purpose IN ('batch_input','batch_output','export','branding','video_output','user_file')),
 -- NULL: installation scope (branding, installation exports).
 workspace_id uuid REFERENCES workspaces(id),
 created_by_user_id uuid REFERENCES users(id),
 created_by_api_key_id uuid,
 -- Original client filename (sanitized: no path, no control characters) and type.
 filename text CHECK(filename IS NULL OR (char_length(filename) BETWEEN 1 AND 255 AND filename !~ '[[:cntrl:]/\\]')),
 content_type text CHECK(content_type IS NULL OR content_type ~ '^[a-z0-9][a-z0-9!#$&^_.+-]{0,63}/[a-z0-9][a-z0-9!#$&^_.+-]{0,63}$'),
 -- Plaintext size and SHA-256, written once the object is fully stored.
 size_bytes bigint CHECK(size_bytes IS NULL OR size_bytes >= 0),
 sha256 bytea CHECK(sha256 IS NULL OR octet_length(sha256) = 32),
 backend text NOT NULL CHECK(backend IN ('local','s3','memory')),
 encryption_key_id text NOT NULL CHECK(encryption_key_id ~ '^[A-Za-z0-9][A-Za-z0-9._-]{0,31}$'),
 created_at timestamptz NOT NULL DEFAULT now(),
 committed_at timestamptz,
 -- Explicit expiry (client-requested or set by the consumer). The purpose
 -- group's retention also applies; whichever comes first wins.
 expires_at timestamptz,
 deleted_at timestamptz,
 delete_attempts integer NOT NULL DEFAULT 0 CHECK(delete_attempts >= 0),
 last_delete_attempt_at timestamptz,
 last_delete_error text CHECK(last_delete_error IS NULL OR last_delete_error ~ '^[a-z_]{1,32}$'),
 FOREIGN KEY(workspace_id, created_by_api_key_id) REFERENCES api_keys(workspace_id, id),
 CHECK(object_key = purpose || '/' || coalesce(workspace_id::text, 'installation') || '/' || id::text),
 CHECK(CASE purpose WHEN 'branding' THEN workspace_id IS NULL WHEN 'export' THEN true ELSE workspace_id IS NOT NULL END),
 CHECK(created_by_api_key_id IS NULL OR workspace_id IS NOT NULL),
 CHECK((committed_at IS NULL) = (size_bytes IS NULL) AND (committed_at IS NULL) = (sha256 IS NULL)),
 CHECK(deleted_at IS NULL OR (filename IS NULL AND content_type IS NULL))
);
-- Per-workspace stored bytes (future storage quota) without a table scan.
CREATE INDEX stored_files_workspace_live ON stored_files(workspace_id) INCLUDE (size_bytes) WHERE deleted_at IS NULL;
-- Retention sweeps and per-purpose totals.
CREATE INDEX stored_files_purpose_live ON stored_files(purpose, created_at) WHERE deleted_at IS NULL;
CREATE INDEX stored_files_expiring ON stored_files(expires_at) WHERE deleted_at IS NULL AND expires_at IS NOT NULL;

CREATE FUNCTION stored_files_guard() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 IF OLD.deleted_at IS NOT NULL THEN
  RAISE EXCEPTION 'deleted stored files are final';
 END IF;
 IF OLD.committed_at IS NOT NULL AND (NEW.committed_at IS DISTINCT FROM OLD.committed_at
   OR NEW.size_bytes IS DISTINCT FROM OLD.size_bytes OR NEW.sha256 IS DISTINCT FROM OLD.sha256) THEN
  RAISE EXCEPTION 'stored file contents are written once';
 END IF;
 IF NEW.delete_attempts < OLD.delete_attempts THEN
  RAISE EXCEPTION 'stored file delete attempts only increase';
 END IF;
 RETURN NEW;
END $$;
CREATE TRIGGER stored_files_guard BEFORE UPDATE ON stored_files FOR EACH ROW EXECUTE FUNCTION stored_files_guard();
