-- Batch engine (0021). Explicit operator upgrade only (`migrate`); never
-- applied implicitly by serve. See docs/batches.md.
--
-- `/v1/batches` runs any model: natively on a provider's batch API when every
-- line routes to one deployment whose adapter supports it, otherwise as a
-- gateway-run batch whose lines go through the normal inference engine, each
-- with its own durable reservation. Everything here is metadata: never batch
-- lines, custom ids, outputs or metadata values.

-- 1. Batch price lists. A v3 price version may carry a second list of price
-- lines that applies to native (provider batch API) execution only. Same
-- validation as `price_lines`; the token ceilings and `max_units` are shared.
-- Rows stay append-only (the 0001 trigger is unchanged).
ALTER TABLE deployment_prices ADD COLUMN batch_price_lines jsonb
 CHECK(batch_price_lines IS NULL OR (pricing_version=3 AND valid_price_lines(batch_price_lines)));

-- 2. The price list a reservation pinned: `batch` only for native batches of
-- a price version that published batch lines. Fixed at admission.
ALTER TABLE governance_reservations
 ADD COLUMN price_tier text NOT NULL DEFAULT 'standard' CHECK(price_tier IN ('standard','batch')),
 ADD CONSTRAINT governance_reservations_batch_tier CHECK(price_tier='standard' OR price_id IS NOT NULL);

-- 3. Async jobs: batch modes, gateway files and runner state.
-- A native batch is created before it is submitted upstream (the runner
-- uploads the gateway's copy), so its upstream id is written once later. A
-- gateway-run batch never has one.
ALTER TABLE async_jobs ALTER COLUMN upstream_id DROP NOT NULL;
ALTER TABLE async_jobs DROP CONSTRAINT async_jobs_batch_endpoint_check;
ALTER TABLE async_jobs ADD CONSTRAINT async_jobs_batch_endpoint_check
 CHECK(batch_endpoint IN ('/v1/chat/completions','/v1/responses','/v1/embeddings','/v1/messages'));
ALTER TABLE async_jobs
 -- NULL: a batch created by the 0016 passthrough (native, provider files).
 ADD COLUMN batch_mode text CHECK(batch_mode IN ('native','gateway')),
 ADD COLUMN user_id uuid REFERENCES users(id),
 -- Client input (stored_files, purpose batch_input), the batch's private copy
 -- (batch_output stored without an API purpose: never listed by /v1/files; the
 -- client may delete its input) and the results (batch_output). Written once.
 ADD COLUMN input_file_id uuid REFERENCES stored_files(id),
 ADD COLUMN work_file_id uuid REFERENCES stored_files(id),
 ADD COLUMN output_file_id uuid REFERENCES stored_files(id),
 ADD COLUMN error_file_id uuid REFERENCES stored_files(id),
 -- Native: whether the batch price list applied ('standard' = no batch price).
 ADD COLUMN price_tier text CHECK(price_tier IN ('standard','batch')),
 -- Explicit per-batch retry policy for failed gateway-run lines (default none).
 ADD COLUMN retry_limit smallint NOT NULL DEFAULT 0 CHECK(retry_limit BETWEEN 0 AND 2),
 -- Gateway-run batches: one runner at a time holds a renewable lease.
 ADD COLUMN runner_id uuid,
 ADD COLUMN runner_lease_until timestamptz,
 -- Native: set before the upstream submission starts (an interrupted
 -- submission is never repeated: the attempt may have reached the provider).
 ADD COLUMN submit_started_at timestamptz,
 ADD COLUMN in_progress_at timestamptz,
 ADD COLUMN finalizing_at timestamptz,
 -- Last time a line finished (or the provider's counts moved): stall alerts.
 ADD COLUMN last_progress_at timestamptz,
 ADD CONSTRAINT async_jobs_upstream_present CHECK(upstream_id IS NOT NULL OR (kind='batch' AND batch_mode IS NOT NULL)),
 ADD CONSTRAINT async_jobs_gateway_local CHECK(batch_mode IS DISTINCT FROM 'gateway' OR upstream_id IS NULL),
 ADD CONSTRAINT async_jobs_engine_files CHECK(batch_mode IS NULL OR (input_file_id IS NOT NULL AND work_file_id IS NOT NULL)),
 ADD CONSTRAINT async_jobs_batch_mode_kind CHECK(batch_mode IS NULL OR kind='batch');
CREATE INDEX async_jobs_gateway_runnable ON async_jobs(runner_lease_until NULLS FIRST) WHERE batch_mode='gateway' AND settled_at IS NULL;
CREATE INDEX async_jobs_active ON async_jobs(workspace_id,created_at DESC) WHERE kind='batch' AND settled_at IS NULL;

CREATE OR REPLACE FUNCTION async_job_guard() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 IF NEW.id<>OLD.id OR NEW.kind<>OLD.kind OR NEW.workspace_id<>OLD.workspace_id OR NEW.api_key_id<>OLD.api_key_id
  OR NEW.deployment_id<>OLD.deployment_id OR NEW.execution_id<>OLD.execution_id OR NEW.public_model<>OLD.public_model
  OR NEW.provider<>OLD.provider OR (OLD.upstream_id IS NOT NULL AND NEW.upstream_id IS DISTINCT FROM OLD.upstream_id)
  OR NEW.created_at<>OLD.created_at
  OR NEW.video_seconds IS DISTINCT FROM OLD.video_seconds OR NEW.batch_endpoint IS DISTINCT FROM OLD.batch_endpoint
  OR NEW.batch_mode IS DISTINCT FROM OLD.batch_mode OR NEW.user_id IS DISTINCT FROM OLD.user_id
  OR NEW.input_file_id IS DISTINCT FROM OLD.input_file_id OR NEW.work_file_id IS DISTINCT FROM OLD.work_file_id
  OR NEW.price_tier IS DISTINCT FROM OLD.price_tier OR NEW.retry_limit<>OLD.retry_limit
  OR (OLD.output_file_id IS NOT NULL AND NEW.output_file_id IS DISTINCT FROM OLD.output_file_id)
  OR (OLD.error_file_id IS NOT NULL AND NEW.error_file_id IS DISTINCT FROM OLD.error_file_id)
  OR (OLD.submit_started_at IS NOT NULL AND NEW.submit_started_at IS DISTINCT FROM OLD.submit_started_at) THEN
  RAISE EXCEPTION 'async job identity is immutable';
 END IF;
 IF NEW.state<>OLD.state AND NOT(
  (OLD.state='queued' AND NEW.state IN ('in_progress','completed','failed','cancelled','expired'))
  OR (OLD.state='in_progress' AND NEW.state IN ('completed','failed','cancelled','expired'))) THEN
  RAISE EXCEPTION 'invalid async job transition % -> %', OLD.state, NEW.state;
 END IF;
 IF OLD.settled_at IS NOT NULL AND NEW.settled_at IS DISTINCT FROM OLD.settled_at THEN
  RAISE EXCEPTION 'async job settlement is recorded once';
 END IF;
 NEW.updated_at:=clock_timestamp();
 RETURN NEW;
END $$;

-- 4. Batch line attempts link to their batch (gateway-run lines). Lines are
-- ordinary executions with their own reservation; the link exempts them from
-- requests/tokens per minute and requests at once (the batch holds one "Jobs
-- at once" slot instead) and lets Logs/Jobs show a batch's cost so far.
ALTER TABLE inference_executions ADD COLUMN batch_job_id uuid;
ALTER TABLE inference_executions ADD CONSTRAINT inference_executions_batch_job
 FOREIGN KEY(workspace_id,batch_job_id) REFERENCES async_jobs(workspace_id,id);
CREATE INDEX executions_batch_job ON inference_executions(batch_job_id) WHERE batch_job_id IS NOT NULL;

-- 5. Per-line state of gateway-run batches (metadata only). A line is claimed
-- by inserting its row, so it can never be executed twice; a row left
-- `running` by a crashed runner becomes `interrupted` (never re-executed;
-- its reservation is reconciled with the hold retained). `segment` is the
-- result segment that holds the line's result (NULL until written).
CREATE TABLE batch_lines(
 job_id uuid NOT NULL,
 workspace_id uuid NOT NULL,
 line_no integer NOT NULL CHECK(line_no BETWEEN 0 AND 49999),
 state text NOT NULL CHECK(state IN ('running','succeeded','failed','interrupted')),
 attempts smallint NOT NULL DEFAULT 1 CHECK(attempts BETWEEN 1 AND 3),
 execution_id uuid,
 status_code smallint CHECK(status_code BETWEEN 100 AND 599),
 error_code text CHECK(error_code ~ '^[a-z_]{1,64}$'),
 segment integer CHECK(segment>=0),
 started_at timestamptz NOT NULL DEFAULT clock_timestamp(),
 finished_at timestamptz,
 PRIMARY KEY(job_id,line_no),
 FOREIGN KEY(workspace_id,job_id) REFERENCES async_jobs(workspace_id,id),
 CHECK((state='running')=(finished_at IS NULL))
);
CREATE FUNCTION batch_line_guard() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 IF NEW.job_id<>OLD.job_id OR NEW.workspace_id<>OLD.workspace_id OR NEW.line_no<>OLD.line_no
  OR NEW.attempts<OLD.attempts OR (OLD.segment IS NOT NULL AND NEW.segment IS DISTINCT FROM OLD.segment) THEN
  RAISE EXCEPTION 'batch line identity is immutable';
 END IF;
 -- running -> finished; a failed line may run again only as a new attempt.
 IF NEW.state<>OLD.state AND NOT((OLD.state='running' AND NEW.state<>'running')
  OR (OLD.state='failed' AND NEW.state='running' AND NEW.attempts=OLD.attempts+1 AND OLD.segment IS NULL)) THEN
  RAISE EXCEPTION 'invalid batch line transition % -> %', OLD.state, NEW.state;
 END IF;
 RETURN NEW;
END $$;
CREATE TRIGGER batch_lines_guard BEFORE UPDATE ON batch_lines FOR EACH ROW EXECUTE FUNCTION batch_line_guard();
CREATE TRIGGER batch_lines_no_delete BEFORE DELETE ON batch_lines FOR EACH ROW EXECUTE FUNCTION immutable_history();

-- Result segments of a gateway-run batch (encrypted internal batch_output
-- files without an API purpose),
-- merged into the output/error files when the batch ends.
CREATE TABLE batch_segments(
 job_id uuid NOT NULL,
 workspace_id uuid NOT NULL,
 seq integer NOT NULL CHECK(seq>=0),
 file_id uuid NOT NULL REFERENCES stored_files(id),
 lines integer NOT NULL CHECK(lines BETWEEN 1 AND 50000),
 created_at timestamptz NOT NULL DEFAULT clock_timestamp(),
 PRIMARY KEY(job_id,seq),
 FOREIGN KEY(workspace_id,job_id) REFERENCES async_jobs(workspace_id,id)
);
CREATE TRIGGER batch_segments_no_update BEFORE UPDATE OR DELETE ON batch_segments FOR EACH ROW EXECUTE FUNCTION immutable_history();

-- 6. Alert rule kinds: a batch failed (or expired), and a batch made no
-- progress for `window_minutes`.
ALTER TABLE alert_rules DROP CONSTRAINT alert_rules_kind_check;
ALTER TABLE alert_rules ADD CONSTRAINT alert_rules_kind_check
 CHECK(kind IN ('budget_threshold','spend_spike','error_rate','provider_failing','batch_failed','batch_stalled'));
ALTER TABLE alert_rules DROP CONSTRAINT alert_rules_check4;
ALTER TABLE alert_rules ADD CONSTRAINT alert_rules_window_kind
 CHECK((kind IN ('error_rate','provider_failing','batch_stalled')) = (window_minutes IS NOT NULL));
ALTER TABLE alert_events DROP CONSTRAINT alert_events_kind_check;
ALTER TABLE alert_events ADD CONSTRAINT alert_events_kind_check
 CHECK(kind IN ('budget_threshold','spend_spike','error_rate','provider_failing','scim_last_admin','batch_failed','batch_stalled'));
