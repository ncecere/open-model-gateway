-- Realtime audio sessions (`GET /v1/realtime`): one execution + reservation per
-- session, per-response usage rows (metadata only) and the realtime-only v3
-- audio-token meters. Explicit operator upgrade only (`migrate`); never applied
-- implicitly by serve.
--
-- Validators compose with the definitions current at this point (0016 and
-- earlier) instead of copying them: each is first cloned to `<name>_base`, and
-- the original (whose OID existing CHECK constraints reference) becomes a
-- wrapper that adds the realtime shapes and otherwise defers to the base.
DO $$ BEGIN
 EXECUTE replace(pg_get_functiondef('public.valid_model_protocols(text[])'::regprocedure),
  'FUNCTION public.valid_model_protocols(','FUNCTION public.valid_model_protocols_base(');
 EXECUTE replace(pg_get_functiondef('public.valid_cost_components(jsonb)'::regprocedure),
  'FUNCTION public.valid_cost_components(','FUNCTION public.valid_cost_components_base(');
 EXECUTE replace(pg_get_functiondef('public.valid_price_lines(jsonb)'::regprocedure),
  'FUNCTION public.valid_price_lines(','FUNCTION public.valid_price_lines_base(');
END $$;

-- `realtime` is its own workload group: a model declares it alone.
CREATE OR REPLACE FUNCTION valid_model_protocols(protocols text[]) RETURNS boolean LANGUAGE sql IMMUTABLE AS $$
 SELECT protocols IS NOT NULL AND (valid_model_protocols_base(protocols) OR protocols=ARRAY['realtime']::text[])
$$;
DO $$ DECLARE d text; BEGIN
 SELECT pg_get_constraintdef(oid) INTO STRICT d FROM pg_constraint
  WHERE conrelid='public.inference_executions'::regclass AND conname='inference_executions_workload_kind_check';
 ALTER TABLE inference_executions DROP CONSTRAINT inference_executions_workload_kind_check;
 EXECUTE format('ALTER TABLE inference_executions ADD CONSTRAINT inference_executions_workload_kind_check CHECK((%s) OR workload_kind=%L)',
  substring(d from '^CHECK \((.*)\)$'), 'realtime');
END $$;

-- Realtime settlements add three audio-token components to a twelve-key v3
-- breakdown (fifteen keys). Everything else is the base rule.
CREATE OR REPLACE FUNCTION valid_cost_components(v jsonb) RETURNS boolean LANGUAGE plpgsql IMMUTABLE AS $$
DECLARE k text; n integer; amount numeric; audio text[]:=ARRAY['input_audio_tokens_microusd','cache_read_audio_tokens_microusd','output_audio_tokens_microusd'];
BEGIN
 IF v IS NULL OR jsonb_typeof(v)<>'object' OR NOT (v ?| audio) THEN RETURN valid_cost_components_base(v); END IF;
 IF NOT (v ?& audio) THEN RETURN false; END IF;
 SELECT count(*) INTO n FROM jsonb_object_keys(v - audio);
 IF n<>12 OR NOT valid_cost_components_base(v - audio) THEN RETURN false; END IF;
 FOREACH k IN ARRAY audio LOOP
  IF NOT valid_i64_string(v->k) THEN RETURN false; END IF;
 END LOOP;
 SELECT sum((value #>> '{}')::numeric) INTO amount FROM jsonb_each(v);
 RETURN amount<=9223372036854775807;
END $$;

-- Audio-token lines (/M tokens, no variants, no prompt tiers, one line per
-- meter) beside a valid base price. Must mirror billing::v3 exactly.
CREATE OR REPLACE FUNCTION valid_price_lines(v jsonb) RETURNS boolean LANGUAGE plpgsql IMMUTABLE AS $$
DECLARE l jsonb; k text; m text; n integer; audio jsonb:='[]'; rest jsonb:='[]'; seen text[]:='{}';
BEGIN
 IF v IS NULL OR jsonb_typeof(v)<>'array' OR jsonb_array_length(v) NOT BETWEEN 1 AND 64 THEN RETURN false; END IF;
 FOR l IN SELECT jsonb_array_elements(v) LOOP
  IF jsonb_typeof(l)='object' AND jsonb_typeof(l->'meter')='string'
   AND l->>'meter' IN ('input_audio_tokens','cache_read_audio_tokens','output_audio_tokens') THEN
   audio:=audio||jsonb_build_array(l);
  ELSE
   rest:=rest||jsonb_build_array(l);
  END IF;
 END LOOP;
 IF jsonb_array_length(audio)=0 THEN RETURN valid_price_lines_base(v); END IF;
 IF jsonb_array_length(rest)=0 OR NOT valid_price_lines_base(rest) THEN RETURN false; END IF;
 FOR l IN SELECT jsonb_array_elements(audio) LOOP
  m:=l->>'meter';
  IF m=ANY(seen) THEN RETURN false; END IF;
  seen:=seen||m;
  FOR k IN SELECT jsonb_object_keys(l) LOOP
   IF NOT(k=ANY(ARRAY['meter','microusd_per_batch','batch','unit_label','sku_label','not_applicable'])) THEN RETURN false; END IF;
  END LOOP;
  SELECT count(*) INTO n FROM jsonb_object_keys(l);
  IF l ? 'not_applicable' THEN
   IF n<>2 OR l->'not_applicable'<>'true'::jsonb THEN RETURN false; END IF;
  ELSE
   IF n<>5 OR NOT valid_i64_string(l->'microusd_per_batch') OR jsonb_typeof(l->'batch')<>'number'
    OR (l->>'batch')::numeric<>1000000 OR jsonb_typeof(l->'unit_label')<>'string' OR l->>'unit_label'<>'/M tokens'
    OR jsonb_typeof(l->'sku_label')<>'string' OR btrim(l->>'sku_label')='' OR char_length(l->>'sku_label')>80
    OR (l->>'sku_label') ~ '[[:cntrl:]]' THEN RETURN false; END IF;
  END IF;
 END LOOP;
 RETURN true;
END $$;

-- One row per realtime response of a session (sequence from 1). Metadata only:
-- no audio, text, instructions, tool arguments, item or upstream ids.
-- Token columns are per modality: `*_tokens` totals include the cached subset.
CREATE TABLE realtime_responses(
 execution_id uuid NOT NULL REFERENCES governance_reservations(execution_id),
 sequence integer NOT NULL CHECK(sequence BETWEEN 1 AND 100000),
 state text NOT NULL DEFAULT 'pending' CHECK(state IN ('pending','settled','unknown')),
 status text CHECK(status IN ('completed','cancelled','incomplete','failed')),
 window_hold_microusd bigint CHECK(window_hold_microusd>=0),
 actual_microusd bigint CHECK(actual_microusd>=0),
 floor_microusd bigint CHECK(floor_microusd>=0),
 unbounded_cost boolean NOT NULL DEFAULT false,
 input_text_tokens bigint CHECK(input_text_tokens>=0),
 cached_text_tokens bigint CHECK(cached_text_tokens>=0),
 input_audio_tokens bigint CHECK(input_audio_tokens>=0),
 cached_audio_tokens bigint CHECK(cached_audio_tokens>=0),
 output_text_tokens bigint CHECK(output_text_tokens>=0),
 output_audio_tokens bigint CHECK(output_audio_tokens>=0),
 cost_components jsonb CHECK(valid_cost_components(cost_components)),
 created_at timestamptz NOT NULL DEFAULT clock_timestamp(),
 completed_at timestamptz,
 PRIMARY KEY(execution_id,sequence),
 CHECK(cached_text_tokens<=input_text_tokens AND cached_audio_tokens<=input_audio_tokens),
 CHECK((state='settled')=(actual_microusd IS NOT NULL)),
 CHECK(state<>'settled' OR (input_text_tokens IS NOT NULL AND cached_text_tokens IS NOT NULL AND input_audio_tokens IS NOT NULL
  AND cached_audio_tokens IS NOT NULL AND output_text_tokens IS NOT NULL AND output_audio_tokens IS NOT NULL)),
 CHECK(cost_components IS NULL OR (actual_microusd IS NOT NULL AND components_total(cost_components)=actual_microusd)),
 CHECK((state='pending')=(completed_at IS NULL))
);
-- A response settles once: pending → settled|unknown; terminal rows never change.
CREATE FUNCTION realtime_response_guard() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 IF NEW.execution_id<>OLD.execution_id OR NEW.sequence<>OLD.sequence OR NEW.created_at<>OLD.created_at
  OR NEW.window_hold_microusd IS DISTINCT FROM OLD.window_hold_microusd OR OLD.state<>'pending' THEN
  RAISE EXCEPTION 'realtime responses settle once';
 END IF;
 RETURN NEW;
END $$;
CREATE TRIGGER realtime_responses_guard BEFORE UPDATE ON realtime_responses FOR EACH ROW EXECUTE FUNCTION realtime_response_guard();
CREATE TRIGGER realtime_responses_no_delete BEFORE DELETE ON realtime_responses FOR EACH ROW EXECUTE FUNCTION immutable_history();
CREATE TRIGGER realtime_responses_no_truncate BEFORE TRUNCATE ON realtime_responses FOR EACH STATEMENT EXECUTE FUNCTION immutable_history();
