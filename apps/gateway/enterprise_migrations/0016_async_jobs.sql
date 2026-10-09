-- Async jobs: video generation and batches (metadata only).
-- Explicit operator upgrade only (`migrate`); never applied implicitly by serve.
--
-- Stores job identity, ownership, state, timestamps, counts and the
-- reservation link. Never prompts, batch lines, metadata values, outputs,
-- error messages or media. See docs/async-jobs.md.

-- Protocols: `videos` and `batches` are separate workload groups.
CREATE OR REPLACE FUNCTION valid_model_protocols(protocols text[]) RETURNS boolean LANGUAGE sql IMMUTABLE AS $$
 SELECT cardinality(protocols) BETWEEN 1 AND 3 AND array_position(protocols,NULL) IS NULL
  AND protocols <@ ARRAY['chat_completions','responses','messages','embeddings','images','audio_transcriptions','audio_speech','rerank','systemone','videos','batches']::text[]
  AND cardinality(protocols)=(SELECT count(DISTINCT p) FROM unnest(protocols) p)
  AND (protocols <@ ARRAY['chat_completions','responses','messages']::text[] OR cardinality(protocols)=1)
$$;
ALTER TABLE inference_executions DROP CONSTRAINT inference_executions_workload_kind_check;
ALTER TABLE inference_executions ADD CONSTRAINT inference_executions_workload_kind_check CHECK(workload_kind IN ('generation','embeddings','images','audio_transcriptions','audio_speech','rerank','systemone','videos','batches'));

-- Meter usage: the six original keys are required; `output_video_seconds_ms`
-- is optional (absent = unknown), so existing evidence stays valid.
CREATE OR REPLACE FUNCTION valid_meter_usage(v jsonb) RETURNS boolean LANGUAGE plpgsql IMMUTABLE AS $$
DECLARE k text; n integer;
BEGIN
 IF v IS NULL THEN RETURN true; END IF;
 IF jsonb_typeof(v)<>'object' THEN RETURN false; END IF;
 SELECT count(*) INTO n FROM jsonb_object_keys(v);
 IF n<>6 AND NOT(n=7 AND v ? 'output_video_seconds_ms') THEN RETURN false; END IF;
 FOR k IN SELECT jsonb_object_keys(v) LOOP
  IF NOT(k=ANY(ARRAY['output_images','input_characters','input_audio_seconds_ms','output_audio_seconds_ms','search_units','requests','output_video_seconds_ms'])) OR NOT valid_i64_string(v->k,true) THEN RETURN false; END IF;
 END LOOP;
 RETURN true;
END $$;
-- Cost components: 6 (v2), 12 (v3) or 13 (v3 with a video component).
CREATE OR REPLACE FUNCTION valid_cost_components(v jsonb) RETURNS boolean LANGUAGE plpgsql IMMUTABLE AS $$
DECLARE k text; n integer; amount numeric:=0; keys text[]:=ARRAY['uncached_input_microusd','cache_read_microusd','cache_write_default_microusd','cache_write_5m_microusd','cache_write_1h_microusd','output_microusd'];
BEGIN
 IF v IS NULL THEN RETURN true; END IF;
 IF jsonb_typeof(v)<>'object' THEN RETURN false; END IF;
 SELECT count(*) INTO n FROM jsonb_object_keys(v);
 IF n IN (12,13) THEN keys:=keys||ARRAY['output_images_microusd','input_characters_microusd','input_audio_microusd','output_audio_microusd','search_units_microusd','requests_microusd'];
 ELSIF n<>6 THEN RETURN false; END IF;
 IF n=13 THEN keys:=keys||ARRAY['output_video_microusd']; END IF;
 FOREACH k IN ARRAY keys LOOP
  IF NOT valid_i64_string(v->k) THEN RETURN false; END IF;
  amount:=amount+(v->>k)::numeric;
 END LOOP;
 RETURN amount<=9223372036854775807;
END $$;
-- Pricing v3 gains `output_video_seconds_ms` (variant = resolution). Must
-- mirror billing::v3 validation exactly.
CREATE OR REPLACE FUNCTION valid_price_lines(v jsonb) RETURNS boolean LANGUAGE plpgsql IMMUTABLE AS $$
DECLARE l jsonb; k text; m text; b numeric; n integer; label text; seen text[]:='{}'; key text; na_meters text[]:='{}'; meters text[]:='{}';
BEGIN
 IF v IS NULL OR jsonb_typeof(v)<>'array' OR jsonb_array_length(v) NOT BETWEEN 1 AND 64 THEN RETURN false; END IF;
 FOR l IN SELECT jsonb_array_elements(v) LOOP
  IF jsonb_typeof(l)<>'object' OR jsonb_typeof(l->'meter')<>'string' THEN RETURN false; END IF;
  m:=l->>'meter';
  IF NOT(m=ANY(ARRAY['input_tokens','output_tokens','cache_read_tokens','cache_write_tokens','cache_write_5m_tokens','cache_write_1h_tokens','output_images','input_characters','input_audio_seconds_ms','output_audio_seconds_ms','search_units','requests','output_video_seconds_ms'])) THEN RETURN false; END IF;
  FOR k IN SELECT jsonb_object_keys(l) LOOP
   IF NOT(k=ANY(ARRAY['meter','microusd_per_batch','batch','unit_label','sku_label','variant','min_prompt_tokens','not_applicable'])) THEN RETURN false; END IF;
  END LOOP;
  SELECT count(*) INTO n FROM jsonb_object_keys(l);
  IF l ? 'not_applicable' THEN
   IF n<>2 OR l->'not_applicable'<>'true'::jsonb THEN RETURN false; END IF;
   na_meters:=na_meters||m;
  ELSE
   IF NOT(l ?& ARRAY['microusd_per_batch','batch','unit_label','sku_label']) OR NOT valid_i64_string(l->'microusd_per_batch') OR jsonb_typeof(l->'batch')<>'number' OR jsonb_typeof(l->'unit_label')<>'string' OR jsonb_typeof(l->'sku_label')<>'string' THEN RETURN false; END IF;
   b:=(l->>'batch')::numeric;
   label:=CASE
    WHEN m LIKE '%\_tokens' AND b=1000000 THEN '/M tokens'
    WHEN m='output_images' AND b=1 THEN '/image'
    WHEN m='input_characters' AND b=1000000 THEN '/M characters'
    WHEN m IN('input_audio_seconds_ms','output_audio_seconds_ms','output_video_seconds_ms') AND b=1000 THEN '/second'
    WHEN m IN('input_audio_seconds_ms','output_audio_seconds_ms','output_video_seconds_ms') AND b=60000 THEN '/minute'
    WHEN m IN('input_audio_seconds_ms','output_audio_seconds_ms','output_video_seconds_ms') AND b=3600000 THEN '/hour'
    WHEN m='search_units' AND b=1 THEN '/search'
    WHEN m='requests' AND b=1 THEN '/request'
   END;
   IF label IS NULL OR l->>'unit_label'<>label THEN RETURN false; END IF;
   IF btrim(l->>'sku_label')='' OR char_length(l->>'sku_label')>80 OR (l->>'sku_label') ~ '[[:cntrl:]]' THEN RETURN false; END IF;
   IF l ? 'variant' AND (m NOT IN ('output_images','output_video_seconds_ms') OR jsonb_typeof(l->'variant')<>'string' OR NOT valid_meter_variant(l->>'variant')) THEN RETURN false; END IF;
   IF l ? 'min_prompt_tokens' AND (jsonb_typeof(l->'min_prompt_tokens')<>'number' OR (l->>'min_prompt_tokens')::numeric<>trunc((l->>'min_prompt_tokens')::numeric) OR (l->>'min_prompt_tokens')::numeric NOT BETWEEN 1 AND 2147483647) THEN RETURN false; END IF;
  END IF;
  key:=m||'|'||coalesce(l->>'variant','')||'|'||coalesce(l->>'min_prompt_tokens','');
  IF key=ANY(seen) THEN RETURN false; END IF;
  seen:=seen||key; meters:=meters||m;
 END LOOP;
 IF EXISTS(SELECT 1 FROM unnest(na_meters) x WHERE (SELECT count(*) FROM unnest(meters) y WHERE y=x)>1) THEN RETURN false; END IF;
 RETURN true;
END $$;
CREATE OR REPLACE FUNCTION valid_max_units(v jsonb) RETURNS boolean LANGUAGE plpgsql IMMUTABLE AS $$
DECLARE k text;
BEGIN
 IF v IS NULL OR jsonb_typeof(v)<>'object' THEN RETURN false; END IF;
 FOR k IN SELECT jsonb_object_keys(v) LOOP
  IF NOT(k=ANY(ARRAY['output_images','input_characters','input_audio_seconds_ms','output_audio_seconds_ms','search_units','requests','output_video_seconds_ms'])) OR NOT valid_i64_string(v->k) THEN RETURN false; END IF;
 END LOOP;
 RETURN true;
END $$;

-- A batch is one upstream attempt covering many requests: its reservation
-- records the request count so settlement checks aggregated usage against
-- `request_count × input_token_limit` (null = one request).
ALTER TABLE governance_reservations ADD COLUMN request_count integer CHECK(request_count BETWEEN 1 AND 50000);

CREATE FUNCTION valid_upstream_job_id(v text) RETURNS boolean LANGUAGE sql IMMUTABLE AS $$ SELECT v ~ '^[A-Za-z0-9._:-]{1,128}$' $$;

CREATE TABLE async_jobs(
 id uuid PRIMARY KEY,
 kind text NOT NULL CHECK(kind IN ('video','batch')),
 workspace_id uuid NOT NULL REFERENCES workspaces(id),
 api_key_id uuid NOT NULL,
 deployment_id uuid NOT NULL REFERENCES deployments(id),
 execution_id uuid NOT NULL UNIQUE,
 public_model text NOT NULL CHECK(char_length(public_model) BETWEEN 1 AND 200),
 provider text NOT NULL CHECK(char_length(provider) BETWEEN 1 AND 64),
 upstream_id text NOT NULL CHECK(valid_upstream_job_id(upstream_id)),
 state text NOT NULL DEFAULT 'queued' CHECK(state IN ('queued','in_progress','completed','failed','cancelled','expired')),
 upstream_status text CHECK(upstream_status ~ '^[a-z_]{1,32}$'),
 progress smallint CHECK(progress BETWEEN 0 AND 100),
 error_code text CHECK(error_code ~ '^[a-z_]{1,64}$'),
 created_at timestamptz NOT NULL DEFAULT clock_timestamp(),
 updated_at timestamptz NOT NULL DEFAULT clock_timestamp(),
 completed_at timestamptz,
 expires_at timestamptz,
 cancel_requested_at timestamptz,
 deleted_at timestamptz,
 poll_deadline_at timestamptz NOT NULL,
 next_poll_at timestamptz NOT NULL DEFAULT clock_timestamp(),
 last_polled_at timestamptz,
 poll_failures integer NOT NULL DEFAULT 0 CHECK(poll_failures>=0),
 settled_at timestamptz,
 video_seconds integer CHECK(video_seconds BETWEEN 1 AND 3600),
 video_size text CHECK(valid_meter_variant(video_size)),
 batch_endpoint text CHECK(batch_endpoint IN ('/v1/chat/completions')),
 request_total integer CHECK(request_total>=0),
 request_completed integer CHECK(request_completed>=0),
 request_failed integer CHECK(request_failed>=0),
 UNIQUE(workspace_id,id),
 UNIQUE(deployment_id,upstream_id),
 FOREIGN KEY(workspace_id,api_key_id) REFERENCES api_keys(workspace_id,id),
 FOREIGN KEY(workspace_id,api_key_id,deployment_id,execution_id) REFERENCES inference_executions(workspace_id,api_key_id,deployment_id,id),
 CHECK((kind='video')=(video_seconds IS NOT NULL AND video_size IS NOT NULL)),
 CHECK((kind='batch')=(batch_endpoint IS NOT NULL)),
 CHECK(settled_at IS NULL OR state IN ('completed','failed','cancelled','expired')),
 CHECK((completed_at IS NULL) OR state IN ('completed','failed','cancelled','expired'))
);
CREATE INDEX async_jobs_workspace_list ON async_jobs(workspace_id,kind,created_at DESC,id DESC);
CREATE INDEX async_jobs_due ON async_jobs(next_poll_at) WHERE settled_at IS NULL;

-- Identity is immutable; state only moves forward (terminal states are final);
-- settlement is recorded once.
CREATE FUNCTION async_job_guard() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 IF NEW.id<>OLD.id OR NEW.kind<>OLD.kind OR NEW.workspace_id<>OLD.workspace_id OR NEW.api_key_id<>OLD.api_key_id
  OR NEW.deployment_id<>OLD.deployment_id OR NEW.execution_id<>OLD.execution_id OR NEW.public_model<>OLD.public_model
  OR NEW.provider<>OLD.provider OR NEW.upstream_id<>OLD.upstream_id OR NEW.created_at<>OLD.created_at
  OR NEW.video_seconds IS DISTINCT FROM OLD.video_seconds OR NEW.batch_endpoint IS DISTINCT FROM OLD.batch_endpoint THEN
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
CREATE TRIGGER async_jobs_guard BEFORE UPDATE ON async_jobs FOR EACH ROW EXECUTE FUNCTION async_job_guard();
CREATE TRIGGER async_jobs_no_delete BEFORE DELETE ON async_jobs FOR EACH ROW EXECUTE FUNCTION immutable_history();

-- Gateway file ids for batch input files (uploaded through the gateway) and
-- the provider's output/error files of a batch. Content is never stored.
CREATE TABLE async_job_files(
 id uuid PRIMARY KEY,
 workspace_id uuid NOT NULL REFERENCES workspaces(id),
 api_key_id uuid NOT NULL,
 deployment_id uuid NOT NULL REFERENCES deployments(id),
 public_model text NOT NULL CHECK(char_length(public_model) BETWEEN 1 AND 200),
 upstream_id text NOT NULL CHECK(valid_upstream_job_id(upstream_id)),
 purpose text NOT NULL CHECK(purpose IN ('batch','batch_output','batch_error')),
 bytes bigint CHECK(bytes>=0),
 endpoint text CHECK(endpoint IN ('/v1/chat/completions')),
 request_count integer CHECK(request_count BETWEEN 1 AND 50000),
 output_token_sum bigint CHECK(output_token_sum>=1),
 max_line_output integer CHECK(max_line_output>=1),
 job_id uuid,
 -- An input file is claimed by one batch attempt before admission; a claim
 -- is released only when nothing was sent upstream.
 claimed_by_execution_id uuid UNIQUE,
 created_at timestamptz NOT NULL DEFAULT clock_timestamp(),
 UNIQUE(deployment_id,upstream_id),
 FOREIGN KEY(workspace_id,api_key_id) REFERENCES api_keys(workspace_id,id),
 FOREIGN KEY(workspace_id,job_id) REFERENCES async_jobs(workspace_id,id),
 CHECK((purpose='batch')=(endpoint IS NOT NULL AND request_count IS NOT NULL AND output_token_sum IS NOT NULL AND max_line_output IS NOT NULL)),
 CHECK(purpose='batch' OR job_id IS NOT NULL),
 CHECK(job_id IS NULL OR purpose<>'batch' OR claimed_by_execution_id IS NOT NULL)
);
-- One input file per batch; one output and one error file per batch.
CREATE UNIQUE INDEX async_job_files_per_job ON async_job_files(job_id,purpose) WHERE job_id IS NOT NULL;
CREATE INDEX async_job_files_workspace ON async_job_files(workspace_id,created_at DESC,id DESC);
-- A file row is immutable except that an input file is consumed once.
CREATE FUNCTION async_job_file_guard() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 IF NEW.id<>OLD.id OR NEW.workspace_id<>OLD.workspace_id OR NEW.api_key_id<>OLD.api_key_id OR NEW.deployment_id<>OLD.deployment_id
  OR NEW.public_model<>OLD.public_model OR NEW.upstream_id<>OLD.upstream_id OR NEW.purpose<>OLD.purpose
  OR NEW.bytes IS DISTINCT FROM OLD.bytes OR NEW.endpoint IS DISTINCT FROM OLD.endpoint OR NEW.request_count IS DISTINCT FROM OLD.request_count
  OR NEW.output_token_sum IS DISTINCT FROM OLD.output_token_sum OR NEW.max_line_output IS DISTINCT FROM OLD.max_line_output
  OR NEW.created_at<>OLD.created_at OR (OLD.job_id IS NOT NULL AND NEW.job_id IS DISTINCT FROM OLD.job_id)
  OR (OLD.job_id IS NOT NULL AND NEW.claimed_by_execution_id IS DISTINCT FROM OLD.claimed_by_execution_id) THEN
  RAISE EXCEPTION 'async job files are immutable';
 END IF;
 RETURN NEW;
END $$;
CREATE TRIGGER async_job_files_guard BEFORE UPDATE ON async_job_files FOR EACH ROW EXECUTE FUNCTION async_job_file_guard();
CREATE TRIGGER async_job_files_no_delete BEFORE DELETE ON async_job_files FOR EACH ROW EXECUTE FUNCTION immutable_history();
