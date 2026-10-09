-- Uploaded installation logo (0023). Explicit operator upgrade only
-- (`migrate`); never applied implicitly by serve. See docs/settings.md#logo.
--
-- The logo is a PNG, JPEG or WebP image stored encrypted in the file store
-- (purpose `branding`, installation scope, never expires) and served
-- same-origin at GET /api/v1/branding/logo. The settings row references the
-- current file; replacing or removing the logo deletes the previous object.
-- The external `logo_url` (0010) stays for API compatibility but is no longer
-- shown anywhere.
ALTER TABLE installation_settings
 ADD COLUMN branding_logo_file_id uuid REFERENCES stored_files(id),
 ADD COLUMN branding_logo_updated_at timestamptz,
 ADD COLUMN branding_logo_width integer CHECK(branding_logo_width BETWEEN 16 AND 4096),
 ADD COLUMN branding_logo_height integer CHECK(branding_logo_height BETWEEN 16 AND 4096),
 ADD CONSTRAINT installation_settings_branding_logo_complete CHECK(
  num_nulls(branding_logo_file_id,branding_logo_updated_at,branding_logo_width,branding_logo_height) IN (0,4));

-- A newly referenced logo must be a live, committed installation branding file.
CREATE FUNCTION installation_settings_logo_guard() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 IF NEW.branding_logo_file_id IS NOT NULL
   AND NEW.branding_logo_file_id IS DISTINCT FROM OLD.branding_logo_file_id
   AND NOT EXISTS(SELECT FROM stored_files f WHERE f.id=NEW.branding_logo_file_id
     AND f.purpose='branding' AND f.workspace_id IS NULL
     AND f.committed_at IS NOT NULL AND f.deleted_at IS NULL) THEN
  RAISE EXCEPTION 'installation logo must be a live branding file';
 END IF;
 RETURN NEW;
END $$;
CREATE TRIGGER installation_settings_logo_guard BEFORE UPDATE OF branding_logo_file_id ON installation_settings
 FOR EACH ROW EXECUTE FUNCTION installation_settings_logo_guard();
