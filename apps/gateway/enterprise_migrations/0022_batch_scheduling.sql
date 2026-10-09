-- Capacity-aware scheduling of gateway-run batch lines (0022). Explicit
-- operator upgrade only (`migrate`); never applied implicitly by serve. See
-- docs/batches.md#scheduling-on-self-hosted-models.
--
-- Lines of gateway-run batches start only when their route has capacity:
-- per-route concurrency, yielding to live traffic, an optional server load
-- signal (vLLM-compatible Prometheus metrics), an optional time window, and
-- fair sharing across workspaces and batches. Native provider batches are
-- unaffected. Everything here is metadata: never lines, prompts or outputs.

-- 1. Per-route settings. A route without a row uses the defaults: at most 2
-- batch lines at once and no other gate.
CREATE TABLE deployment_batch_scheduling(
 deployment_id uuid PRIMARY KEY REFERENCES deployments(id),
 max_concurrency integer NOT NULL DEFAULT 2 CHECK(max_concurrency BETWEEN 1 AND 256),
 -- Start lines only while fewer live (non-batch) requests are in flight.
 yield_live_threshold integer CHECK(yield_live_threshold BETWEEN 1 AND 100000),
 -- Server load signal: an approved local origin's /metrics (validated by the
 -- server against GATEWAY_LOCAL_UPSTREAMS; fetched with pinned addresses).
 metrics_url text CHECK(char_length(metrics_url) BETWEEN 12 AND 2048 AND metrics_url ~ '^https?://[^[:space:]?#@]+/metrics$'),
 metrics_max_waiting integer CHECK(metrics_max_waiting BETWEEN 0 AND 100000),
 metrics_max_running integer CHECK(metrics_max_running BETWEEN 0 AND 100000),
 metrics_max_kv_cache_percent smallint CHECK(metrics_max_kv_cache_percent BETWEEN 1 AND 100),
 -- vLLM `priority` sent on batch lines only (lower is earlier; live traffic
 -- uses 0, so batch lines are always later).
 priority integer CHECK(priority BETWEEN 1 AND 1000000),
 -- Allowed days (bit 0 = Monday … bit 6 = Sunday, the day a span starts)
 -- and local start/end minutes in an IANA time zone; end <= start spans
 -- midnight, end = start is the whole day.
 window_timezone text CHECK(char_length(window_timezone) BETWEEN 1 AND 64),
 window_days smallint CHECK(window_days BETWEEN 1 AND 127),
 window_start_minute smallint CHECK(window_start_minute BETWEEN 0 AND 1439),
 window_end_minute smallint CHECK(window_end_minute BETWEEN 0 AND 1439),
 updated_at timestamptz NOT NULL DEFAULT clock_timestamp(),
 updated_by uuid REFERENCES users(id),
 CONSTRAINT deployment_batch_scheduling_metrics CHECK((metrics_url IS NULL) = (num_nonnulls(metrics_max_waiting,metrics_max_running,metrics_max_kv_cache_percent)=0)),
 CONSTRAINT deployment_batch_scheduling_window CHECK(num_nulls(window_timezone,window_days,window_start_minute,window_end_minute) IN (0,4))
);

-- 2. The last gate evaluation of a route (any gateway process): pause reason,
-- live in-flight count and the last server metrics reading.
CREATE TABLE deployment_batch_signals(
 deployment_id uuid PRIMARY KEY REFERENCES deployments(id),
 paused_reason text CHECK(paused_reason IN ('outside_window','live_traffic','server_busy','metrics_unavailable')),
 checked_at timestamptz NOT NULL,
 live_in_flight integer CHECK(live_in_flight>=0),
 metrics_checked_at timestamptz,
 metrics_ok boolean,
 metrics_waiting bigint CHECK(metrics_waiting>=0),
 metrics_running bigint CHECK(metrics_running>=0),
 metrics_kv_cache_permille integer CHECK(metrics_kv_cache_permille BETWEEN 0 AND 1000),
 metrics_error text CHECK(metrics_error ~ '^[a-z_]{1,64}$')
);

-- 3. Demand: a gateway-run batch with lines waiting for a route, heartbeated
-- by its runner (stale rows are ignored). `ready` means its runner would
-- start a line now; fair sharing orders ready batches by their workspace's
-- running lines, then their own, then who was served least recently.
-- Scheduling state, not history: rows are replaced and removed.
CREATE TABLE batch_route_waits(
 job_id uuid NOT NULL,
 workspace_id uuid NOT NULL,
 deployment_id uuid NOT NULL REFERENCES deployments(id),
 waiting_lines integer NOT NULL CHECK(waiting_lines BETWEEN 0 AND 50000),
 reason text CHECK(reason IN ('outside_window','live_traffic','server_busy','metrics_unavailable','concurrency','fair_share','workers','rate_limited')),
 ready boolean NOT NULL DEFAULT false,
 since timestamptz NOT NULL DEFAULT clock_timestamp(),
 last_claim_at timestamptz,
 updated_at timestamptz NOT NULL DEFAULT clock_timestamp(),
 PRIMARY KEY(deployment_id,job_id),
 FOREIGN KEY(workspace_id,job_id) REFERENCES async_jobs(workspace_id,id)
);
CREATE INDEX batch_route_waits_job ON batch_route_waits(job_id);

-- 4. A line's route is fixed when it is claimed (the claim checks the route's
-- capacity under a per-route lock). NULL: claimed before 0022, or a line
-- without a route (it fails in the engine like an interactive request).
ALTER TABLE batch_lines ADD COLUMN deployment_id uuid REFERENCES deployments(id);
CREATE INDEX batch_lines_running_route ON batch_lines(deployment_id,workspace_id) WHERE state='running';
CREATE INDEX batch_lines_running_job ON batch_lines(job_id) WHERE state='running';
CREATE OR REPLACE FUNCTION batch_line_guard() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 IF NEW.job_id<>OLD.job_id OR NEW.workspace_id<>OLD.workspace_id OR NEW.line_no<>OLD.line_no
  OR NEW.attempts<OLD.attempts OR (OLD.segment IS NOT NULL AND NEW.segment IS DISTINCT FROM OLD.segment)
  OR NEW.deployment_id IS DISTINCT FROM OLD.deployment_id THEN
  RAISE EXCEPTION 'batch line identity is immutable';
 END IF;
 -- running -> finished; a failed line may run again only as a new attempt.
 IF NEW.state<>OLD.state AND NOT((OLD.state='running' AND NEW.state<>'running')
  OR (OLD.state='failed' AND NEW.state='running' AND NEW.attempts=OLD.attempts+1 AND OLD.segment IS NULL)) THEN
  RAISE EXCEPTION 'invalid batch line transition % -> %', OLD.state, NEW.state;
 END IF;
 RETURN NEW;
END $$;

-- 5. Batches: the completion window (hours; NULL = 24 for 0021 batches and
-- non-batch jobs), fixed at creation, and the last time the runner saw the
-- batch legitimately waiting for its routes (window, live traffic, server
-- load, concurrency): the stall alert's clock does not run while it waits.
ALTER TABLE async_jobs
 ADD COLUMN completion_window_hours smallint CHECK(completion_window_hours IN (24,48,72,168)),
 ADD COLUMN last_waited_at timestamptz,
 ADD CONSTRAINT async_jobs_window_kind CHECK(completion_window_hours IS NULL OR kind='batch');

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
  OR NEW.completion_window_hours IS DISTINCT FROM OLD.completion_window_hours
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
