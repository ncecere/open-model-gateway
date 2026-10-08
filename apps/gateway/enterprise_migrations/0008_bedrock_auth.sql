-- Bedrock authentication modes and an optional allowlisted VPC endpoint.
-- Explicit operator upgrade only (`migrate`); never applied implicitly by serve.
--
-- credential_ref remains a reference, never a secret. Bedrock connections may use:
--   aws:default                       the server's default AWS credential chain
--   aws:profile:<name>                a named profile from the server's AWS config files
--   aws:role:<role-arn>[;external_id=<id>][;session_name=<name>]
--                                     STS AssumeRole called with the server identity
-- The optional role parts are stored in that fixed order; none of the ARN, external ID
-- or session name character sets contains ';'. Server allowlists
-- (GATEWAY_AWS_PROFILE_ALLOWLIST, GATEWAY_BEDROCK_ENDPOINT_ALLOWLIST) are enforced by the
-- application at write time and again at execution; the database checks shape only.
-- endpoint stays NULL for the regional Bedrock Runtime endpoint; otherwise it is an exact
-- HTTPS origin (no credentials, path, query or fragment).
DO $$ DECLARE c record; dropped integer := 0; BEGIN
 FOR c IN SELECT conname FROM pg_constraint
  WHERE conrelid='public.provider_connections'::regclass AND contype='c'
   AND pg_get_constraintdef(oid) LIKE '%aws:default%' LOOP
  EXECUTE format('ALTER TABLE public.provider_connections DROP CONSTRAINT %I', c.conname);
  dropped := dropped + 1;
 END LOOP;
 -- The credential-reference shape check and the Bedrock pairing check from 0001.
 IF dropped <> 2 THEN RAISE EXCEPTION 'unexpected provider_connections constraints (% matched)', dropped; END IF;
END $$;
ALTER TABLE provider_connections
 ADD CONSTRAINT provider_connections_credential_ref_shape CHECK(
  credential_ref IN ('none','aws:default')
  OR credential_ref ~ '^env:[A-Z_][A-Z0-9_]*$'
  OR credential_ref ~ '^aws:profile:[A-Za-z0-9_.+][A-Za-z0-9_.+-]{0,63}$'
  -- PostgreSQL bounds repetition counts at 255; the overall length bounds the external ID
  -- (2-1224 characters, enforced exactly by the application).
  OR (char_length(credential_ref) <= 4096
   AND credential_ref ~ '^aws:role:arn:aws(-[a-z]+)*:iam::[0-9]{12}:role/([A-Za-z0-9_+=,.@-]+/)*[A-Za-z0-9_+=,.@-]{1,64}(;external_id=[A-Za-z0-9_+=,.@:/-]{2,})?(;session_name=[A-Za-z0-9_+=,.@-]{2,64})?$')),
 ADD CONSTRAINT provider_connections_bedrock_auth CHECK((provider='bedrock')=(credential_ref LIKE 'aws:%')),
 ADD CONSTRAINT provider_connections_bedrock_endpoint CHECK(
  provider<>'bedrock' OR endpoint IS NULL
  OR endpoint ~ '^https://[A-Za-z0-9.-]+(:[0-9]{1,5})?$');
