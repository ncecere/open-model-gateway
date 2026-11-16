-- Gateway-run batch claim epoch (0081, #638). Explicit operator upgrade only
-- (`migrate`); never applied implicitly by serve.
--
-- A runner claims a batch by writing its id and a lease term to the job row
-- (0021). The claim now also bumps `runner_epoch`, and every runner write
-- checks `(runner_id, runner_epoch)` against the row inside the transaction
-- that writes (`jobs/fence.rs`), holding the job row `FOR SHARE` (or
-- `FOR NO KEY UPDATE` when it also writes the row), so a takeover (whose
-- claim is `FOR UPDATE SKIP LOCKED`) waits for the write and a runner that
-- lost the batch can never commit one more write to it.
--
-- Rolling update: the column has a default and nothing reads or writes it
-- except the new claim, renewal and fence statements. A release before this
-- one keeps claiming with `runner_id` and the lease only; the new fence also
-- compares `runner_id`, so a batch taken over by an older process still fences
-- the new process that lost it. An older process that loses a batch to a new
-- one is not fenced (it does not know the epoch).
--
-- Locks: ADD COLUMN with a constant default is a catalog-only change (no table
-- rewrite or scan; ACCESS EXCLUSIVE on `async_jobs` for an instant). CREATE
-- TRIGGER takes SHARE ROW EXCLUSIVE on `async_jobs` (blocks concurrent writes,
-- not reads, and scans nothing) until this migration's transaction commits.
-- No CHECK constraint and no index: a validated CHECK would scan the table
-- under the ACCESS EXCLUSIVE lock the column added, and the fence reads the
-- row by primary key.
ALTER TABLE async_jobs ADD COLUMN runner_epoch bigint NOT NULL DEFAULT 0;

-- Epochs only move forward (like work_leases.epoch).
CREATE FUNCTION async_job_epoch_guard() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 IF NEW.runner_epoch<OLD.runner_epoch THEN
  RAISE EXCEPTION 'batch runner epochs only move forward' USING ERRCODE='23514';
 END IF;
 RETURN NEW;
END $$;
CREATE TRIGGER async_jobs_epoch_forward BEFORE UPDATE OF runner_epoch ON async_jobs
 FOR EACH ROW EXECUTE FUNCTION async_job_epoch_guard();
