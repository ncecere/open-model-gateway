-- Work leases for multi-replica background work (scale plan P5).
-- Explicit operator upgrade only (`migrate`); never applied implicitly by serve.
--
-- Singleton background jobs (account lifecycle cleanup, detail compaction,
-- rate-counter pruning and storage-usage recording, alert evaluation, the
-- stored-file sweeper, the installation-wide reservation gauge) run on
-- exactly one replica at a time: the holder of the job's lease row.
--   * omg_lease_acquire(name, holder, ttl) takes a free or expired lease, or
--     renews the caller's own; it returns the lease's epoch (fencing token) or
--     NULL. A new term (another holder, or the same holder after expiry)
--     always gets a larger epoch; epochs never decrease (trigger).
--   * omg_lease_fence(name, holder, epoch), the first statement of every job
--     transaction, fails unless that term is still current and unexpired,
--     and holds the lease row FOR SHARE until commit, so a takeover waits for
--     the running job transaction and a paused former leader can never commit
--     work after its term ended.
--   * omg_lease_release(name, holder, epoch) expires the caller's term at
--     shutdown, so another replica takes over at its next attempt.
-- Queues (expired-reservation reconciliation, async job polling, batch lines,
-- alert deliveries) keep FOR UPDATE SKIP LOCKED claims on every replica.
-- Lease names are seeded here; the runtime cannot create, rename or remove
-- them.

CREATE TABLE work_leases(
 name text PRIMARY KEY CHECK(name ~ '^[a-z][a-z0-9_]{0,62}$'),
 holder uuid,
 epoch bigint NOT NULL DEFAULT 0 CHECK(epoch>=0),
 acquired_at timestamptz,
 expires_at timestamptz NOT NULL DEFAULT '-infinity',
 last_completed_at timestamptz,
 last_completed_epoch bigint
);
INSERT INTO work_leases(name) VALUES('alerts'),('compaction'),('file_sweep'),('lifecycle'),('maintenance'),('metrics');

CREATE FUNCTION work_leases_guard() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 IF TG_OP IN ('DELETE','TRUNCATE') THEN
  RAISE EXCEPTION 'work leases are never removed' USING ERRCODE='23514';
 END IF;
 IF NEW.name<>OLD.name OR NEW.epoch<OLD.epoch OR NEW.last_completed_epoch<OLD.last_completed_epoch THEN
  RAISE EXCEPTION 'work lease epochs only move forward' USING ERRCODE='23514';
 END IF;
 RETURN NEW;
END $$;
CREATE TRIGGER work_leases_forward BEFORE UPDATE OR DELETE ON work_leases
 FOR EACH ROW EXECUTE FUNCTION work_leases_guard();
CREATE TRIGGER work_leases_no_truncate BEFORE TRUNCATE ON work_leases
 FOR EACH STATEMENT EXECUTE FUNCTION work_leases_guard();

-- Take or renew `lease` for `me` for `ttl_seconds` (1..3600). Returns the
-- term's epoch, or NULL while another holder's term is unexpired.
CREATE FUNCTION omg_lease_acquire(lease text, me uuid, ttl_seconds integer) RETURNS bigint LANGUAGE plpgsql AS $$
DECLARE e bigint;
BEGIN
 IF me IS NULL OR ttl_seconds IS NULL OR ttl_seconds NOT BETWEEN 1 AND 3600 THEN
  RAISE EXCEPTION 'invalid work lease request';
 END IF;
 UPDATE work_leases w SET
   epoch=CASE WHEN w.holder=me AND w.expires_at>clock_timestamp() THEN w.epoch ELSE w.epoch+1 END,
   acquired_at=CASE WHEN w.holder=me AND w.expires_at>clock_timestamp() THEN w.acquired_at ELSE clock_timestamp() END,
   holder=me,
   expires_at=clock_timestamp()+make_interval(secs=>ttl_seconds)
  WHERE w.name=lease AND (w.holder=me OR w.holder IS NULL OR w.expires_at<=clock_timestamp())
  RETURNING w.epoch INTO e;
 RETURN e;
END $$;

-- Fence a job transaction to `epoch` of `lease` held by `me`.
CREATE FUNCTION omg_lease_fence(lease text, me uuid, term bigint) RETURNS void LANGUAGE plpgsql AS $$
BEGIN
 PERFORM 1 FROM work_leases w WHERE w.name=lease AND w.holder=me AND w.epoch=term
  AND w.expires_at>clock_timestamp() FOR SHARE;
 IF NOT FOUND THEN
  RAISE EXCEPTION 'work lease % term % is no longer held',lease,term USING ERRCODE='55006';
 END IF;
END $$;

-- Record a completed run of the current term (fenced: only the current,
-- unexpired term can record). Returns whether it was recorded.
CREATE FUNCTION omg_lease_complete(lease text, me uuid, term bigint) RETURNS boolean LANGUAGE plpgsql AS $$
BEGIN
 UPDATE work_leases w SET last_completed_at=clock_timestamp(),last_completed_epoch=term
  WHERE w.name=lease AND w.holder=me AND w.epoch=term AND w.expires_at>clock_timestamp();
 RETURN FOUND;
END $$;

-- End `me`'s term of `lease` now (graceful shutdown).
CREATE FUNCTION omg_lease_release(lease text, me uuid, term bigint) RETURNS boolean LANGUAGE plpgsql AS $$
BEGIN
 UPDATE work_leases w SET expires_at='-infinity'
  WHERE w.name=lease AND w.holder=me AND w.epoch=term;
 RETURN FOUND;
END $$;
