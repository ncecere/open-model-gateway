-- Autovacuum of the admission-read reservation index (scale follow-up to P2/P6b).
-- Explicit operator upgrade only (`migrate`); never applied implicitly by serve.
-- Metadata only: no rewrite, no data change. ALTER TABLE ... SET takes SHARE
-- UPDATE EXCLUSIVE on each partition (reads and writes continue; it waits
-- for a running vacuum of that partition).
--
-- Every admission subtracts its workspace's pending reservations whose lease
-- already expired (0024, P2): an index range scan of `governance_leases`
-- (lease_expires_at) WHERE state='pending' up to the admission instant. A
-- settlement changes state, so every settled reservation leaves a dead entry
-- in that range until vacuum removes it, and every admission walks over all
-- of them. With the defaults, autovacuum of a month partition waits for 20 %
-- of its rows to be dead (a 2.6 M-row partition: about 520 k), and a vacuum
-- that finds dead tuples on fewer than 2 % of the pages skips index cleanup
-- altogether (PostgreSQL's index-vacuum bypass). Measured (P6b, 2 M history,
-- 1 replica): 435 k dead tuples made the expired-lease read 0.57 instead of
-- 0.17 ms and saturation 4-7 % lower than after a manual VACUUM.
--
-- governance_reservations leaf partitions (existing and future):
--   autovacuum_vacuum_scale_factor=0.01, autovacuum_vacuum_threshold=10000:
--     vacuum after 10 k + 1 % dead rows instead of 20 %, whatever the size;
--   vacuum_index_cleanup=on: always clean the indexes (no bypass), so the
--     dead governance_leases entries actually go away;
--   autovacuum_vacuum_cost_limit=2000, autovacuum_vacuum_cost_delay=2: a
--     vacuum finishes in seconds instead of being throttled for minutes.
-- Partitioned parents cannot hold storage parameters, so they are set on
-- every leaf partition here, and omg_ensure_partitions (the partitions job)
-- applies omg_partition_storage() to each month partition it creates. A
-- detached and re-attached (archived) partition keeps its own settings.
-- The counter and totals tables already use threshold autovacuum (0024,
-- 0025); inference_executions and monetary_ledger keep the defaults.
SET lock_timeout = '60s';

-- Storage parameters of new month partitions, per registered parent (NULL:
-- PostgreSQL defaults).
CREATE FUNCTION omg_partition_storage(parent text) RETURNS text LANGUAGE sql IMMUTABLE PARALLEL SAFE AS $$
 SELECT CASE parent
  WHEN 'governance_reservations' THEN 'autovacuum_vacuum_scale_factor=0.01,autovacuum_vacuum_threshold=10000,vacuum_index_cleanup=on,autovacuum_vacuum_cost_limit=2000,autovacuum_vacuum_cost_delay=2'
 END
$$;
REVOKE ALL ON FUNCTION omg_partition_storage(text) FROM PUBLIC;

DO $$
DECLARE r record;
BEGIN
 FOR r IN SELECT c.relname, omg_partition_storage(p.relname) opts FROM pg_inherits h
   JOIN pg_class c ON c.oid=h.inhrelid JOIN pg_class p ON p.oid=h.inhparent
   WHERE p.relnamespace='public'::regnamespace AND c.relkind='r'
    AND omg_partition_storage(p.relname) IS NOT NULL ORDER BY c.relname LOOP
  EXECUTE format('ALTER TABLE public.%I SET (%s)', r.relname, r.opts);
 END LOOP;
END $$;

-- 0030's omg_ensure_partitions, plus the storage parameters (set on the new,
-- empty table before it is attached).
CREATE OR REPLACE FUNCTION omg_ensure_partitions(ahead integer, at timestamptz)
RETURNS TABLE(parent text, partition text) LANGUAGE plpgsql SECURITY DEFINER
SET search_path=pg_catalog,public,pg_temp SET lock_timeout='5s' AS $$
DECLARE h record; m timestamptz; last timestamptz; name text; opts text;
BEGIN
 IF ahead IS NULL OR ahead NOT BETWEEN 0 AND 12 OR at IS NULL THEN
  RAISE EXCEPTION 'partitions are created 0..12 months ahead' USING ERRCODE='22023';
 END IF;
 last := omg_month_start(at);
 FOR i IN 1..ahead LOOP
  last := omg_next_month(last);
 END LOOP;
 FOR h IN SELECT p.parent FROM public.history_partitions p ORDER BY p.position LOOP
  CONTINUE WHEN (SELECT c.relkind FROM pg_class c WHERE c.relnamespace='public'::regnamespace AND c.relname=h.parent) IS DISTINCT FROM 'p';
  m := omg_month_start(at);
  WHILE m<=last LOOP
   IF NOT EXISTS(SELECT 1 FROM omg_partition_bounds(h.parent) b WHERE b.lower_bound<=m AND b.upper_bound>=omg_next_month(m)) THEN
    IF EXISTS(SELECT 1 FROM omg_partition_bounds(h.parent) b WHERE b.lower_bound<omg_next_month(m) AND b.upper_bound>m) THEN
     RAISE EXCEPTION 'partition of % overlaps month % only partly; repair manually', h.parent, m USING ERRCODE='42P17';
    END IF;
    name := h.parent||'_p'||to_char(m AT TIME ZONE 'UTC','YYYY_MM');
    EXECUTE format('CREATE TABLE public.%I (LIKE public.%I INCLUDING DEFAULTS INCLUDING CONSTRAINTS INCLUDING GENERATED INCLUDING STORAGE)', name, h.parent);
    opts := public.omg_partition_storage(h.parent);
    IF opts IS NOT NULL THEN
     EXECUTE format('ALTER TABLE public.%I SET (%s)', name, opts);
    END IF;
    EXECUTE format('ALTER TABLE public.%I ATTACH PARTITION public.%I FOR VALUES FROM (%L) TO (%L)', h.parent, name, m, omg_next_month(m));
    EXECUTE format('CREATE TRIGGER %I BEFORE TRUNCATE ON public.%I FOR EACH STATEMENT EXECUTE FUNCTION public.immutable_history()', name||'_no_truncate', name);
    parent := h.parent;
    partition := name;
    RETURN NEXT;
   END IF;
   m := omg_next_month(m);
  END LOOP;
 END LOOP;
END $$;
