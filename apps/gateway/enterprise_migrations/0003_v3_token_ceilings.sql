-- Pricing v3 workloads without input-token meters (speech, transcription) may
-- publish an input token ceiling of 0. The application additionally requires
-- every input-family token meter (input, cache read, cache writes) to be
-- explicitly not_applicable for a zero ceiling; v1/v2 keep a positive ceiling.
-- Explicit operator upgrade only (`migrate`); never applied implicitly by serve.
DO $$ DECLARE c record; BEGIN
 FOR c IN SELECT conname FROM pg_constraint WHERE conrelid='public.deployment_prices'::regclass AND contype='c' AND pg_get_constraintdef(oid) LIKE '%input_token_limit > 0%' LOOP
  EXECUTE format('ALTER TABLE public.deployment_prices DROP CONSTRAINT %I',c.conname);
 END LOOP;
END $$;
ALTER TABLE deployment_prices ADD CONSTRAINT deployment_prices_input_token_limit_check CHECK(input_token_limit>0 OR (pricing_version=3 AND input_token_limit=0));
