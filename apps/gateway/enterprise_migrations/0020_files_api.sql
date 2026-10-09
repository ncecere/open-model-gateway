-- Files API, storage quota and storage usage (0020). Explicit operator upgrade
-- only (`migrate`); never applied implicitly by serve. See docs/files-api.md,
-- docs/governance.md#storage and docs/file-storage.md.

-- 1. Files API metadata on stored_files (0019).
--
-- api_purpose: the OpenAI purpose a client used (`batch`, `user_data`,
-- `vision`, `assistants`, `evals`) or `batch_output` for files written by the
-- batch engine. Written once: at insert, or at commit for an upload whose
-- form sent the file before its purpose (trigger); NULL for files the Files
-- API never shows (exports, branding, video outputs).
--
-- reserved_bytes: the storage-quota reservation of an upload in progress.
-- A pending (uncommitted, undeleted) row counts its reservation against the
-- workspace quota; a committed row counts its size. It only grows while the
-- row is pending and never changes afterwards (trigger).
ALTER TABLE stored_files
 ADD COLUMN api_purpose text CHECK(api_purpose IS NULL OR api_purpose IN ('batch','batch_output','user_data','vision','assistants','evals')),
 ADD COLUMN reserved_bytes bigint NOT NULL DEFAULT 0 CHECK(reserved_bytes >= 0),
 ADD CONSTRAINT stored_files_api_purpose_shape CHECK(api_purpose IS NULL OR CASE api_purpose
   WHEN 'batch' THEN purpose='batch_input'
   WHEN 'batch_output' THEN purpose='batch_output'
   ELSE purpose='user_file' END);
-- Quota: live bytes and reservations per workspace without a table scan.
CREATE INDEX stored_files_workspace_quota ON stored_files(workspace_id, created_at)
 INCLUDE (size_bytes, reserved_bytes, committed_at, expires_at, purpose) WHERE deleted_at IS NULL;
-- Files API listing (newest first per workspace).
CREATE INDEX stored_files_api_list ON stored_files(workspace_id, created_at, id)
 WHERE deleted_at IS NULL AND api_purpose IS NOT NULL;
-- Storage usage: files deleted inside an hour being recorded.
CREATE INDEX stored_files_deleted ON stored_files(deleted_at) WHERE deleted_at IS NOT NULL AND workspace_id IS NOT NULL;

CREATE OR REPLACE FUNCTION stored_files_guard() RETURNS trigger LANGUAGE plpgsql AS $$
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
 IF NEW.reserved_bytes <> OLD.reserved_bytes AND (OLD.committed_at IS NOT NULL OR NEW.reserved_bytes < OLD.reserved_bytes) THEN
  RAISE EXCEPTION 'stored file reservations only grow while pending';
 END IF;
 IF NEW.api_purpose IS DISTINCT FROM OLD.api_purpose AND (OLD.committed_at IS NOT NULL OR OLD.api_purpose IS NOT NULL) THEN
  RAISE EXCEPTION 'stored file api purpose is written once';
 END IF;
 RETURN NEW;
END $$;

-- 2. "Storage" quota (`storage_bytes`): bytes a workspace may hold in the file
-- store (committed files plus live upload reservations, every purpose). Same
-- stacked layers as "Jobs at once" (0018) minus the installation and key
-- layers: workspace-type default, platform per-workspace override (replaces
-- the type default) and tighten-only workspace local. NULL inherits / no cap.
-- Type defaults start at 1 GiB per workspace; every kind gets a row so the
-- default applies on fresh installations.
ALTER TABLE workspace_type_policies ADD COLUMN storage_bytes bigint DEFAULT 1073741824 CHECK(storage_bytes>0);
INSERT INTO workspace_type_policies(kind) VALUES('personal'),('team'),('project') ON CONFLICT(kind) DO NOTHING;
ALTER TABLE workspace_platform_policy_overrides ADD COLUMN storage_bytes bigint CHECK(storage_bytes>0);
ALTER TABLE workspace_local_policies ADD COLUMN storage_bytes bigint CHECK(storage_bytes>0);

-- 3. Storage usage (not charged). Append-only hourly rows of byte-seconds per
-- workspace and purpose, derived from stored_files timestamps (committed_at
-- to deleted_at) by the maintenance task. Quantities only: no price, no cost.
-- A later optional storage price (micro-USD per GB-day, effective-dated,
-- append-only) applies to hours starting at or after its effective time;
-- earlier rows stay "Not charged" and are never rewritten.
CREATE TABLE storage_usage_hours(
 workspace_id uuid NOT NULL REFERENCES workspaces(id),
 purpose text NOT NULL CHECK(purpose IN ('batch_input','batch_output','export','video_output','user_file')),
 hour_start timestamptz NOT NULL CHECK(extract(epoch FROM hour_start) % 3600 = 0),
 byte_seconds numeric(38,0) NOT NULL CHECK(byte_seconds > 0),
 -- Files of this workspace and purpose stored at some point in the hour.
 file_count integer NOT NULL CHECK(file_count > 0),
 recorded_at timestamptz NOT NULL DEFAULT now(),
 PRIMARY KEY(workspace_id, purpose, hour_start)
);
CREATE INDEX storage_usage_hours_time ON storage_usage_hours(hour_start);
-- Hours before recorded_through are complete; it only moves forward.
CREATE TABLE storage_usage_progress(
 singleton boolean PRIMARY KEY DEFAULT true CHECK(singleton),
 recorded_through timestamptz NOT NULL CHECK(extract(epoch FROM recorded_through) % 3600 = 0)
);
INSERT INTO storage_usage_progress(recorded_through)
 SELECT coalesce(date_trunc('hour', min(committed_at), 'UTC'), date_trunc('hour', now(), 'UTC'))
 FROM stored_files WHERE workspace_id IS NOT NULL;

CREATE FUNCTION storage_usage_guard() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 IF TG_TABLE_NAME = 'storage_usage_hours' THEN
  RAISE EXCEPTION 'storage usage history is append-only';
 END IF;
 IF TG_OP = 'DELETE' OR NEW.recorded_through < OLD.recorded_through THEN
  RAISE EXCEPTION 'storage usage progress only moves forward';
 END IF;
 RETURN NEW;
END $$;
CREATE TRIGGER storage_usage_hours_guard BEFORE UPDATE OR DELETE ON storage_usage_hours FOR EACH ROW EXECUTE FUNCTION storage_usage_guard();
CREATE TRIGGER storage_usage_progress_guard BEFORE UPDATE OR DELETE ON storage_usage_progress FOR EACH ROW EXECUTE FUNCTION storage_usage_guard();
