-- Multimodal workloads, meter usage evidence and immutable pricing v3 price lines.
-- Explicit operator upgrade only (`migrate`); never applied implicitly by serve.

-- 1a. Protocols are nonempty, distinct and within one workload group:
-- chat_completions/responses/messages together, or exactly one other kind.
CREATE OR REPLACE FUNCTION valid_model_protocols(protocols text[]) RETURNS boolean LANGUAGE sql IMMUTABLE AS $$
 SELECT cardinality(protocols) BETWEEN 1 AND 3 AND array_position(protocols,NULL) IS NULL
  AND protocols <@ ARRAY['chat_completions','responses','messages','embeddings','images','audio_transcriptions','audio_speech','rerank','systemone']::text[]
  AND cardinality(protocols)=(SELECT count(DISTINCT p) FROM unnest(protocols) p)
  AND (protocols <@ ARRAY['chat_completions','responses','messages']::text[] OR cardinality(protocols)=1)
$$;
-- Revalidate existing rows: mixed generation+embeddings models must be split first.
ALTER TABLE models DROP CONSTRAINT models_supported_protocols_check;
ALTER TABLE models ADD CONSTRAINT models_supported_protocols_check CHECK(valid_model_protocols(supported_protocols));
ALTER TABLE inference_executions DROP CONSTRAINT inference_executions_workload_kind_check;
ALTER TABLE inference_executions ADD CONSTRAINT inference_executions_workload_kind_check CHECK(workload_kind IN ('generation','embeddings','images','audio_transcriptions','audio_speech','rerank','systemone'));

-- 1b. Presence-preserving meter counters: all six keys, each null or an i64 string.
CREATE FUNCTION valid_meter_usage(v jsonb) RETURNS boolean LANGUAGE plpgsql IMMUTABLE AS $$
DECLARE k text; n integer;
BEGIN
 IF v IS NULL THEN RETURN true; END IF;
 IF jsonb_typeof(v)<>'object' THEN RETURN false; END IF;
 SELECT count(*) INTO n FROM jsonb_object_keys(v); IF n<>6 THEN RETURN false; END IF;
 FOR k IN SELECT jsonb_object_keys(v) LOOP
  IF NOT(k=ANY(ARRAY['output_images','input_characters','input_audio_seconds_ms','output_audio_seconds_ms','search_units','requests'])) OR NOT valid_i64_string(v->k,true) THEN RETURN false; END IF;
 END LOOP;
 RETURN true;
END $$;
CREATE FUNCTION valid_meter_variant(v text) RETURNS boolean LANGUAGE sql IMMUTABLE AS $$ SELECT v IS NULL OR v ~ '^[A-Za-z0-9._:-]{1,32}$' $$;
-- V2 components have six token keys; v3 adds six meter keys (twelve total).
CREATE OR REPLACE FUNCTION valid_cost_components(v jsonb) RETURNS boolean LANGUAGE plpgsql IMMUTABLE AS $$
DECLARE k text; n integer; amount numeric:=0; keys text[]:=ARRAY['uncached_input_microusd','cache_read_microusd','cache_write_default_microusd','cache_write_5m_microusd','cache_write_1h_microusd','output_microusd'];
BEGIN
 IF v IS NULL THEN RETURN true; END IF;
 IF jsonb_typeof(v)<>'object' THEN RETURN false; END IF;
 SELECT count(*) INTO n FROM jsonb_object_keys(v);
 IF n=12 THEN keys:=keys||ARRAY['output_images_microusd','input_characters_microusd','input_audio_microusd','output_audio_microusd','search_units_microusd','requests_microusd'];
 ELSIF n<>6 THEN RETURN false; END IF;
 FOREACH k IN ARRAY keys LOOP
  IF NOT valid_i64_string(v->k) THEN RETURN false; END IF;
  amount:=amount+(v->>k)::numeric;
 END LOOP;
 RETURN amount<=9223372036854775807;
END $$;
ALTER TABLE inference_executions ADD COLUMN meter_usage jsonb CHECK(valid_meter_usage(meter_usage)), ADD COLUMN output_image_variant text CHECK(valid_meter_variant(output_image_variant)), ADD COLUMN provider_cost_microusd bigint CHECK(provider_cost_microusd>=0);
ALTER TABLE governance_reservations ADD COLUMN meter_usage jsonb CHECK(valid_meter_usage(meter_usage)), ADD COLUMN output_image_variant text CHECK(valid_meter_variant(output_image_variant)), ADD COLUMN provider_cost_microusd bigint CHECK(provider_cost_microusd>=0);
ALTER TABLE monetary_ledger ADD COLUMN meter_usage jsonb CHECK(valid_meter_usage(meter_usage)), ADD COLUMN output_image_variant text CHECK(valid_meter_variant(output_image_variant)), ADD COLUMN provider_cost_microusd bigint CHECK(provider_cost_microusd>=0);

-- 1c. Pricing v3. Must mirror billing::v3 validation exactly.
CREATE FUNCTION valid_price_lines(v jsonb) RETURNS boolean LANGUAGE plpgsql IMMUTABLE AS $$
DECLARE l jsonb; k text; m text; b numeric; n integer; label text; seen text[]:='{}'; key text; na_meters text[]:='{}'; meters text[]:='{}';
BEGIN
 IF v IS NULL OR jsonb_typeof(v)<>'array' OR jsonb_array_length(v) NOT BETWEEN 1 AND 64 THEN RETURN false; END IF;
 FOR l IN SELECT jsonb_array_elements(v) LOOP
  IF jsonb_typeof(l)<>'object' OR jsonb_typeof(l->'meter')<>'string' THEN RETURN false; END IF;
  m:=l->>'meter';
  IF NOT(m=ANY(ARRAY['input_tokens','output_tokens','cache_read_tokens','cache_write_tokens','cache_write_5m_tokens','cache_write_1h_tokens','output_images','input_characters','input_audio_seconds_ms','output_audio_seconds_ms','search_units','requests'])) THEN RETURN false; END IF;
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
    WHEN m IN('input_audio_seconds_ms','output_audio_seconds_ms') AND b=1000 THEN '/second'
    WHEN m IN('input_audio_seconds_ms','output_audio_seconds_ms') AND b=60000 THEN '/minute'
    WHEN m IN('input_audio_seconds_ms','output_audio_seconds_ms') AND b=3600000 THEN '/hour'
    WHEN m='search_units' AND b=1 THEN '/search'
    WHEN m='requests' AND b=1 THEN '/request'
   END;
   IF label IS NULL OR l->>'unit_label'<>label THEN RETURN false; END IF;
   IF btrim(l->>'sku_label')='' OR char_length(l->>'sku_label')>80 OR (l->>'sku_label') ~ '[[:cntrl:]]' THEN RETURN false; END IF;
   IF l ? 'variant' AND (m<>'output_images' OR jsonb_typeof(l->'variant')<>'string' OR NOT valid_meter_variant(l->>'variant')) THEN RETURN false; END IF;
   IF l ? 'min_prompt_tokens' AND (jsonb_typeof(l->'min_prompt_tokens')<>'number' OR (l->>'min_prompt_tokens')::numeric<>trunc((l->>'min_prompt_tokens')::numeric) OR (l->>'min_prompt_tokens')::numeric NOT BETWEEN 1 AND 2147483647) THEN RETURN false; END IF;
  END IF;
  key:=m||'|'||coalesce(l->>'variant','')||'|'||coalesce(l->>'min_prompt_tokens','');
  IF key=ANY(seen) THEN RETURN false; END IF;
  seen:=seen||key; meters:=meters||m;
 END LOOP;
 -- A not-applicable line is the only line for its meter.
 IF EXISTS(SELECT 1 FROM unnest(na_meters) x WHERE (SELECT count(*) FROM unnest(meters) y WHERE y=x)>1) THEN RETURN false; END IF;
 RETURN true;
END $$;
CREATE FUNCTION valid_max_units(v jsonb) RETURNS boolean LANGUAGE plpgsql IMMUTABLE AS $$
DECLARE k text;
BEGIN
 IF v IS NULL OR jsonb_typeof(v)<>'object' THEN RETURN false; END IF;
 FOR k IN SELECT jsonb_object_keys(v) LOOP
  IF NOT(k=ANY(ARRAY['output_images','input_characters','input_audio_seconds_ms','output_audio_seconds_ms','search_units','requests'])) OR NOT valid_i64_string(v->k) THEN RETURN false; END IF;
 END LOOP;
 RETURN true;
END $$;
ALTER TABLE deployment_prices ADD COLUMN price_lines jsonb, ADD COLUMN max_units jsonb;
-- Replace the version checks; v3 keeps token rates only in price_lines.
DO $$ DECLARE c record; BEGIN
 FOR c IN SELECT conname FROM pg_constraint WHERE conrelid='public.deployment_prices'::regclass AND contype='c' AND pg_get_constraintdef(oid) LIKE '%pricing_version%' LOOP
  EXECUTE format('ALTER TABLE public.deployment_prices DROP CONSTRAINT %I',c.conname);
 END LOOP;
END $$;
ALTER TABLE deployment_prices ALTER COLUMN input_microusd_per_million DROP NOT NULL, ALTER COLUMN output_microusd_per_million DROP NOT NULL;
ALTER TABLE deployment_prices ADD CONSTRAINT deployment_prices_pricing_version_check CHECK(pricing_version IN (1,2,3));
ALTER TABLE deployment_prices ADD CONSTRAINT deployment_prices_version_shape CHECK(
 (pricing_version=1 AND cache_pricing IS NULL AND price_lines IS NULL AND max_units IS NULL AND input_microusd_per_million IS NOT NULL AND output_microusd_per_million IS NOT NULL)
 OR (pricing_version=2 AND valid_cache_pricing(cache_pricing) AND price_lines IS NULL AND max_units IS NULL AND input_microusd_per_million IS NOT NULL AND output_microusd_per_million IS NOT NULL)
 OR (pricing_version=3 AND cache_pricing IS NULL AND input_microusd_per_million IS NULL AND output_microusd_per_million IS NULL AND valid_price_lines(price_lines) AND valid_max_units(max_units)));
