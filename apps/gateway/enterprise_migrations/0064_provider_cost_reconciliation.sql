-- #48 provider-reported cost reconciliation, NOT a verified invoice.
-- 0058-0063 are reserved; merge/apply only after that ordered prefix exists.
-- Additive evidence only: no changes to estimates, holds, prices or rollups.
CREATE TABLE provider_cost_imports (
 id uuid PRIMARY KEY,
 provider text NOT NULL DEFAULT 'anthropic' CHECK(provider='anthropic'),
 account_id text NOT NULL CHECK(octet_length(account_id) BETWEEN 1 AND 200),
 parser_version text NOT NULL DEFAULT 'anthropic-cost-v1' CHECK(parser_version='anthropic-cost-v1'),
 period_start timestamptz NOT NULL CHECK(period_start=date_trunc('day',period_start,'UTC')),
 period_end timestamptz NOT NULL CHECK(period_end=date_trunc('day',period_end,'UTC') AND period_end>period_start AND period_end<=period_start+interval '744 hours'),
 exported_at timestamptz NOT NULL,
 imported_at timestamptz NOT NULL DEFAULT clock_timestamp(),
 actor_user_id uuid NOT NULL REFERENCES users(id),
 evidence_reference text NOT NULL CHECK(octet_length(evidence_reference) BETWEEN 1 AND 256),
 evidence_json text NOT NULL CHECK(octet_length(evidence_json)<=5242880),
 fingerprint bytea NOT NULL CHECK(octet_length(fingerprint)=32),
 supersedes uuid UNIQUE REFERENCES provider_cost_imports(id),
 revision integer NOT NULL DEFAULT 1 CHECK(revision>0),
 line_count integer NOT NULL CHECK(line_count BETWEEN 0 AND 50000),
 UNIQUE(account_id,fingerprint),
 UNIQUE(account_id,id),
 CHECK(exported_at>=period_end)
);
CREATE TABLE provider_cost_lines (
 import_id uuid NOT NULL REFERENCES provider_cost_imports(id),
 line_number integer NOT NULL CHECK(line_number BETWEEN 1 AND 50000),
 source_page integer NOT NULL CHECK(source_page BETWEEN 0 AND 30),
 source_pointer text NOT NULL CHECK(octet_length(source_pointer)<=128),
 bucket_start timestamptz NOT NULL CHECK(bucket_start=date_trunc('day',bucket_start,'UTC')),
 bucket_end timestamptz NOT NULL CHECK(bucket_end=bucket_start+interval '24 hours'),
 dimensions jsonb NOT NULL CHECK(jsonb_typeof(dimensions)='object'),
 original_amount text NOT NULL CHECK(octet_length(original_amount) BETWEEN 1 AND 64),
 amount_microusd numeric(39,0) NOT NULL,
 conversion_residual_microusd text NOT NULL CHECK(octet_length(conversion_residual_microusd)<=32),
 dimension_key text NOT NULL CHECK(dimension_key ~ '^[0-9a-f]{64}$'),
 PRIMARY KEY(import_id,line_number), UNIQUE(import_id,dimension_key)
);
CREATE TABLE provider_cost_mappings (
 id uuid PRIMARY KEY,
 account_id text NOT NULL CHECK(octet_length(account_id) BETWEEN 1 AND 200),
 supersedes uuid UNIQUE REFERENCES provider_cost_mappings(id),
 revision integer NOT NULL DEFAULT 1 CHECK(revision>0),
 evidence_reference text NOT NULL CHECK(octet_length(evidence_reference) BETWEEN 1 AND 256),
 actor_user_id uuid NOT NULL REFERENCES users(id),
 created_at timestamptz NOT NULL DEFAULT clock_timestamp(),
 binding_count integer NOT NULL CHECK(binding_count BETWEEN 0 AND 1000),
 UNIQUE(account_id,id)
);
ALTER TABLE deployments ADD CONSTRAINT deployments_provider_cost_scope UNIQUE(provider_connection_id,id);
CREATE TABLE provider_cost_bindings (
 mapping_id uuid NOT NULL REFERENCES provider_cost_mappings(id),
 binding_number integer NOT NULL CHECK(binding_number BETWEEN 1 AND 1000),
 provider_workspace_id text CHECK(octet_length(provider_workspace_id) BETWEEN 1 AND 4096),
 connection_id uuid NOT NULL REFERENCES provider_connections(id),
 deployment_id uuid REFERENCES deployments(id),
 workspace_id uuid REFERENCES workspaces(id),
 effective_start timestamptz NOT NULL,
 effective_end timestamptz NOT NULL CHECK(effective_end>effective_start),
 exclusive boolean NOT NULL,
 non_omg_traffic boolean NOT NULL,
 attested boolean NOT NULL CHECK(attested),
 evidence_reference text NOT NULL CHECK(octet_length(evidence_reference) BETWEEN 1 AND 256),
 model_mappings jsonb NOT NULL CHECK(jsonb_typeof(model_mappings)='array' AND jsonb_array_length(model_mappings)<=100),
 PRIMARY KEY(mapping_id,binding_number),
 FOREIGN KEY(connection_id,deployment_id) REFERENCES deployments(provider_connection_id,id),
 CHECK(NOT exclusive OR (NOT non_omg_traffic AND workspace_id IS NOT NULL))
);
CREATE TABLE provider_reconciliations (
 id uuid PRIMARY KEY,
 account_id text NOT NULL,
 import_id uuid NOT NULL,
 mapping_id uuid NOT NULL,
 query_version text NOT NULL DEFAULT 'aggregate-attempt-start-v1' CHECK(query_version='aggregate-attempt-start-v1'),
 observed_at timestamptz NOT NULL DEFAULT clock_timestamp(),
 actor_user_id uuid NOT NULL REFERENCES users(id),
 snapshot jsonb NOT NULL CHECK(jsonb_typeof(snapshot)='object'),
 FOREIGN KEY(account_id,import_id) REFERENCES provider_cost_imports(account_id,id),
 FOREIGN KEY(account_id,mapping_id) REFERENCES provider_cost_mappings(account_id,id)
);

-- Account-scoped serialization also protects privileged/manual INSERTs. No
-- historical credential inference, implicit revision sum or branching lineage.
CREATE FUNCTION provider_cost_revision_guard() RETURNS trigger LANGUAGE plpgsql SET search_path=public,pg_temp AS $$
DECLARE previous record; overlaps boolean;
BEGIN
 PERFORM pg_advisory_xact_lock(hashtextextended('provider-cost:'||NEW.account_id,72419548));
 IF TG_TABLE_NAME='provider_cost_imports' THEN
  IF NEW.supersedes IS NULL THEN
   SELECT EXISTS(SELECT FROM provider_cost_imports WHERE account_id=NEW.account_id AND period_start<NEW.period_end AND period_end>NEW.period_start) INTO overlaps;
   IF overlaps THEN RAISE EXCEPTION 'overlapping provider report requires exact supersession' USING ERRCODE='23514'; END IF;
   NEW.revision:=1;
  ELSE
   SELECT * INTO previous FROM provider_cost_imports WHERE id=NEW.supersedes;
   IF NOT FOUND OR previous.account_id<>NEW.account_id OR previous.period_start<>NEW.period_start OR previous.period_end<>NEW.period_end
     OR EXISTS(SELECT FROM provider_cost_imports WHERE supersedes=NEW.supersedes) THEN
    RAISE EXCEPTION 'invalid provider report lineage' USING ERRCODE='23514';
   END IF;
   NEW.revision:=previous.revision+1;
  END IF;
 ELSE
  IF NEW.supersedes IS NULL THEN
   IF EXISTS(SELECT FROM provider_cost_mappings WHERE account_id=NEW.account_id) THEN RAISE EXCEPTION 'mapping requires supersession' USING ERRCODE='23514'; END IF;
   NEW.revision:=1;
  ELSE
   SELECT * INTO previous FROM provider_cost_mappings WHERE id=NEW.supersedes;
   IF NOT FOUND OR previous.account_id<>NEW.account_id OR EXISTS(SELECT FROM provider_cost_mappings WHERE supersedes=NEW.supersedes) THEN
    RAISE EXCEPTION 'invalid provider mapping lineage' USING ERRCODE='23514';
   END IF;
   NEW.revision:=previous.revision+1;
  END IF;
 END IF;
 RETURN NEW;
END $$;
CREATE TRIGGER provider_cost_import_revision BEFORE INSERT ON provider_cost_imports FOR EACH ROW EXECUTE FUNCTION provider_cost_revision_guard();
CREATE TRIGGER provider_cost_mapping_revision BEFORE INSERT ON provider_cost_mappings FOR EACH ROW EXECUTE FUNCTION provider_cost_revision_guard();

-- A revision is complete at commit; SELECT/INSERT cannot append extra evidence
-- to an already frozen parent later. Parent/child cardinality is fixed forever.
CREATE FUNCTION provider_cost_complete_guard() RETURNS trigger LANGUAGE plpgsql SET search_path=public,pg_temp AS $$
DECLARE parent_id uuid; expected integer; actual bigint;
BEGIN
 IF TG_TABLE_NAME IN ('provider_cost_imports','provider_cost_lines') THEN
  IF TG_TABLE_NAME='provider_cost_imports' THEN parent_id:=NEW.id; ELSE parent_id:=NEW.import_id; END IF;
  SELECT line_count INTO expected FROM provider_cost_imports WHERE id=parent_id;
  SELECT count(*) INTO actual FROM provider_cost_lines WHERE import_id=parent_id;
 ELSE
  IF TG_TABLE_NAME='provider_cost_mappings' THEN parent_id:=NEW.id; ELSE parent_id:=NEW.mapping_id; END IF;
  SELECT binding_count INTO expected FROM provider_cost_mappings WHERE id=parent_id;
  SELECT count(*) INTO actual FROM provider_cost_bindings WHERE mapping_id=parent_id;
 END IF;
 IF expected IS NULL OR actual<>expected THEN RAISE EXCEPTION 'incomplete immutable provider cost revision' USING ERRCODE='23514'; END IF;
 RETURN NULL;
END $$;
CREATE CONSTRAINT TRIGGER provider_cost_import_complete AFTER INSERT ON provider_cost_imports DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION provider_cost_complete_guard();
CREATE CONSTRAINT TRIGGER provider_cost_mapping_complete AFTER INSERT ON provider_cost_mappings DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION provider_cost_complete_guard();
-- Numbers occupy the complete 1..count range (PK + bounds + deferred parent
-- count). Later INSERTs therefore cannot change an already committed revision.
-- Check once per child, count once per parent: not quadratic for 50k lines.
CREATE FUNCTION provider_cost_child_guard() RETURNS trigger LANGUAGE plpgsql SET search_path=public,pg_temp AS $$
DECLARE expected integer; parent_start timestamptz; parent_end timestamptz;
BEGIN
 IF TG_TABLE_NAME='provider_cost_lines' THEN
  SELECT line_count,period_start,period_end INTO expected,parent_start,parent_end FROM provider_cost_imports WHERE id=NEW.import_id;
  IF expected IS NULL OR NEW.line_number>expected OR NEW.bucket_start<parent_start OR NEW.bucket_end>parent_end THEN
   RAISE EXCEPTION 'invalid immutable provider cost line' USING ERRCODE='23514';
  END IF;
 ELSE
  SELECT binding_count INTO expected FROM provider_cost_mappings WHERE id=NEW.mapping_id;
  IF expected IS NULL OR NEW.binding_number>expected THEN RAISE EXCEPTION 'invalid immutable provider cost binding' USING ERRCODE='23514'; END IF;
 END IF;
 RETURN NEW;
END $$;
CREATE TRIGGER provider_cost_line_bounds BEFORE INSERT ON provider_cost_lines FOR EACH ROW EXECUTE FUNCTION provider_cost_child_guard();
CREATE TRIGGER provider_cost_binding_bounds BEFORE INSERT ON provider_cost_bindings FOR EACH ROW EXECUTE FUNCTION provider_cost_child_guard();
DO $$ DECLARE t text; BEGIN
 FOREACH t IN ARRAY ARRAY['provider_cost_imports','provider_cost_lines','provider_cost_mappings','provider_cost_bindings','provider_reconciliations'] LOOP
  EXECUTE format('CREATE TRIGGER %I BEFORE UPDATE OR DELETE ON %I FOR EACH ROW EXECUTE FUNCTION immutable_history()',t||'_immutable',t);
  EXECUTE format('CREATE TRIGGER %I BEFORE TRUNCATE ON %I FOR EACH STATEMENT EXECUTE FUNCTION immutable_history()',t||'_no_truncate',t);
 END LOOP;
END $$;
