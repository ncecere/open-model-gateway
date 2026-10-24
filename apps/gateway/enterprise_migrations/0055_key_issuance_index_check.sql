-- #253: repair for 0054. If an earlier CREATE INDEX CONCURRENTLY was
-- interrupted, an INVALID api_keys_workspace_created is left behind (and 0054's
-- IF NOT EXISTS skips it): drop it and build it (in this transaction, so key
-- writes, not reads, wait while it builds; this only happens after a failed
-- build). Normally the index is already valid and this does nothing.
DO $$ BEGIN
 IF EXISTS(SELECT FROM pg_index i JOIN pg_class c ON c.oid=i.indexrelid JOIN pg_namespace n ON n.oid=c.relnamespace
           WHERE n.nspname='public' AND c.relname='api_keys_workspace_created' AND NOT i.indisvalid) THEN
  DROP INDEX public.api_keys_workspace_created;
 END IF;
 IF to_regclass('public.api_keys_workspace_created') IS NULL THEN
  CREATE INDEX api_keys_workspace_created ON public.api_keys(workspace_id, created_at);
 END IF;
END $$;
