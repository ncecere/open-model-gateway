-- Gateway-run batch claim epoch (0081, #638). Explicit operator upgrade only
-- (`migrate`); never applied implicitly by serve.
--
-- A runner claims a batch by writing its id and a lease term to the job row
-- (0021). The claim now also bumps `runner_epoch`, and every runner write
-- checks `(runner_id, runner_epoch)` against the row inside the transaction
-- that writes (`jobs/fence.rs`), locking the job row `FOR KEY SHARE` (or `FOR
-- NO KEY UPDATE` when it also writes the row). The claim is `FOR UPDATE SKIP
-- LOCKED`, which conflicts with both: it skips a batch with a write in flight
-- and takes it on a later tick, and a runner that lost the batch can never
-- commit one more write to it.
--
-- Upgrade: drain the gateway first (docs/releases/unreleased.md). A gateway
-- serves only a database whose migrations match its own, so a release before
-- this one reports not ready once this migration commits; its batch runner,
-- if it is still running, does not know the epoch and is not fenced. Rollback
-- is a restore of the pre-upgrade backup.
--
-- Locks: ADD COLUMN with a constant default is a catalog-only change (no table
-- rewrite or scan), but it takes ACCESS EXCLUSIVE on `async_jobs`, and the
-- migrator sets no `lock_timeout`: the statement queues behind open
-- transactions on the table, and everything else queues behind it. CREATE
-- TRIGGER takes SHARE ROW EXCLUSIVE (blocks writers, not readers, scans
-- nothing) until this migration's transaction commits. No CHECK constraint and
-- no index: a validated CHECK would scan the table under the lock the column
-- added, and the fence reads the row by primary key.
ALTER TABLE async_jobs ADD COLUMN runner_epoch bigint NOT NULL DEFAULT 0;

-- Epochs only move forward (like work_leases.epoch).
CREATE FUNCTION async_job_epoch_guard() RETURNS trigger LANGUAGE plpgsql SET search_path = pg_catalog, public AS $$
BEGIN
 IF NEW.runner_epoch<OLD.runner_epoch THEN
  RAISE EXCEPTION 'batch runner epochs only move forward' USING ERRCODE='23514';
 END IF;
 RETURN NEW;
END $$;
CREATE TRIGGER async_jobs_epoch_forward BEFORE UPDATE OF runner_epoch ON async_jobs
 FOR EACH ROW EXECUTE FUNCTION async_job_epoch_guard();
