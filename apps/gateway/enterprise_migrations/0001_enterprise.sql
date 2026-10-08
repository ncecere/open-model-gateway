-- Fresh single-installation lineage. Never apply this to a legacy database.
CREATE TABLE installation(singleton boolean PRIMARY KEY DEFAULT true CHECK(singleton), id uuid NOT NULL UNIQUE, name text NOT NULL, schema_family text NOT NULL DEFAULT 'enterprise_v1' CHECK(schema_family='enterprise_v1'), created_at timestamptz NOT NULL DEFAULT now());
INSERT INTO installation(singleton,id,name) VALUES(true,gen_random_uuid(),'Enterprise');
CREATE FUNCTION lock_installation() RETURNS void LANGUAGE sql AS $$ SELECT singleton FROM installation WHERE singleton FOR NO KEY UPDATE $$;
CREATE TABLE users(id uuid PRIMARY KEY, email text, disabled_at timestamptz, cleanup_due_at timestamptz, cleaned_at timestamptz, disable_reason text, oidc_link_allowed boolean NOT NULL DEFAULT false, created_at timestamptz NOT NULL DEFAULT now());
CREATE UNIQUE INDEX users_email_case_insensitive ON users(lower(email));
CREATE TABLE oidc_identities(issuer text NOT NULL, subject text NOT NULL, user_id uuid NOT NULL REFERENCES users(id) ON DELETE RESTRICT, PRIMARY KEY(issuer,subject));
CREATE TABLE platform_role_grants(id uuid PRIMARY KEY DEFAULT gen_random_uuid(), user_id uuid NOT NULL REFERENCES users(id), role text NOT NULL CHECK(role IN ('user','auditor','admin')), source text NOT NULL CHECK(source IN ('manual','group','bootstrap')), mapping_id uuid, granted_by uuid REFERENCES users(id), revoked_at timestamptz, created_at timestamptz NOT NULL DEFAULT now(), CHECK((source='group')=(mapping_id IS NOT NULL)));
CREATE UNIQUE INDEX active_platform_grant ON platform_role_grants(user_id,source,coalesce(mapping_id,'00000000-0000-0000-0000-000000000000'::uuid),role) WHERE revoked_at IS NULL;
CREATE VIEW effective_platform_roles AS SELECT g.user_id, CASE max(CASE g.role WHEN 'admin' THEN 3 WHEN 'auditor' THEN 2 ELSE 1 END) WHEN 3 THEN 'admin' WHEN 2 THEN 'auditor' ELSE 'user' END AS role FROM platform_role_grants g JOIN users u ON u.id=g.user_id WHERE g.revoked_at IS NULL AND u.disabled_at IS NULL AND u.cleaned_at IS NULL GROUP BY g.user_id;
CREATE TABLE cost_centers(id uuid PRIMARY KEY, name text NOT NULL, code text NOT NULL UNIQUE, archived_at timestamptz, created_at timestamptz NOT NULL DEFAULT now());
CREATE TABLE workspaces(id uuid PRIMARY KEY, name text NOT NULL, kind text NOT NULL CHECK(kind IN ('personal','team','project')), owner_user_id uuid REFERENCES users(id), cost_center_id uuid REFERENCES cost_centers(id), disabled_at timestamptz, created_at timestamptz NOT NULL DEFAULT now(), CHECK((kind='personal')=(owner_user_id IS NOT NULL)));
CREATE UNIQUE INDEX active_personal_owner ON workspaces(owner_user_id) WHERE kind='personal' AND disabled_at IS NULL;
CREATE TABLE oidc_group_mappings(id uuid PRIMARY KEY, issuer text NOT NULL, group_value text NOT NULL, target_kind text NOT NULL CHECK(target_kind IN ('platform','workspace')), platform_role text CHECK(platform_role IN ('user','auditor','admin')), workspace_id uuid REFERENCES workspaces(id), workspace_role text CHECK(workspace_role IN ('admin','member')), enabled boolean NOT NULL DEFAULT true, created_at timestamptz NOT NULL DEFAULT now(), CHECK((target_kind='platform' AND platform_role IS NOT NULL AND workspace_id IS NULL AND workspace_role IS NULL) OR (target_kind='workspace' AND platform_role IS NULL AND workspace_id IS NOT NULL AND workspace_role IS NOT NULL)));
-- mapping ids are retained as provenance even after a mapping is deleted.
CREATE TABLE workspace_membership_grants(id uuid PRIMARY KEY DEFAULT gen_random_uuid(), workspace_id uuid NOT NULL REFERENCES workspaces(id), user_id uuid NOT NULL REFERENCES users(id), role text NOT NULL CHECK(role IN ('owner','admin','member')), source text NOT NULL CHECK(source IN ('manual','group')), mapping_id uuid, revoked_at timestamptz, created_at timestamptz NOT NULL DEFAULT now(), CHECK((source='group')=(mapping_id IS NOT NULL)), CHECK(source<>'group' OR role<>'owner'));
CREATE UNIQUE INDEX active_workspace_grant ON workspace_membership_grants(workspace_id,user_id,source,coalesce(mapping_id,'00000000-0000-0000-0000-000000000000'::uuid)) WHERE revoked_at IS NULL;
CREATE VIEW effective_workspace_memberships AS SELECT g.workspace_id,g.user_id,CASE max(CASE g.role WHEN 'owner' THEN 3 WHEN 'admin' THEN 2 ELSE 1 END) WHEN 3 THEN 'owner' WHEN 2 THEN 'admin' ELSE 'member' END AS role FROM workspace_membership_grants g JOIN users u ON u.id=g.user_id JOIN workspaces w ON w.id=g.workspace_id JOIN effective_platform_roles p ON p.user_id=u.id WHERE g.revoked_at IS NULL AND w.disabled_at IS NULL AND (w.kind<>'personal' OR w.owner_user_id=u.id) GROUP BY g.workspace_id,g.user_id;
CREATE TABLE oidc_login_attempts(state_hash bytea PRIMARY KEY CHECK(octet_length(state_hash)=32), browser_hash bytea NOT NULL CHECK(octet_length(browser_hash)=32), nonce text NOT NULL, pkce_verifier text NOT NULL, expires_at timestamptz NOT NULL);
CREATE TABLE browser_sessions(token_hash bytea PRIMARY KEY CHECK(octet_length(token_hash)=32), user_id uuid NOT NULL REFERENCES users(id), csrf_hash bytea NOT NULL CHECK(octet_length(csrf_hash)=32), verified_email text, created_at timestamptz NOT NULL DEFAULT now(), expires_at timestamptz NOT NULL, revoked_at timestamptz);
CREATE INDEX sessions_user ON browser_sessions(user_id);
CREATE TABLE workspace_invitations(id uuid PRIMARY KEY, workspace_id uuid NOT NULL REFERENCES workspaces(id), email text NOT NULL, role text NOT NULL CHECK(role IN ('admin','member')), token_hash bytea NOT NULL UNIQUE CHECK(octet_length(token_hash)=32), expires_at timestamptz NOT NULL, accepted_at timestamptz, revoked_at timestamptz, created_by uuid NOT NULL REFERENCES users(id), created_at timestamptz NOT NULL DEFAULT now());
CREATE TABLE service_accounts(id uuid PRIMARY KEY, workspace_id uuid NOT NULL REFERENCES workspaces(id), name text NOT NULL, disabled_at timestamptz, created_at timestamptz NOT NULL DEFAULT now(), UNIQUE(workspace_id,id));
CREATE FUNCTION require_shared_workspace() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NOT EXISTS(SELECT 1 FROM workspaces WHERE id=NEW.workspace_id AND kind IN ('team','project')) THEN RAISE EXCEPTION 'shared workspace required' USING ERRCODE='23514'; END IF; RETURN NEW; END $$;
CREATE TRIGGER service_account_shared BEFORE INSERT OR UPDATE ON service_accounts FOR EACH ROW EXECUTE FUNCTION require_shared_workspace();
CREATE TRIGGER group_mapping_shared BEFORE INSERT OR UPDATE ON oidc_group_mappings FOR EACH ROW WHEN (NEW.target_kind='workspace') EXECUTE FUNCTION require_shared_workspace();
CREATE TRIGGER invitation_shared BEFORE INSERT OR UPDATE ON workspace_invitations FOR EACH ROW EXECUTE FUNCTION require_shared_workspace();
CREATE TABLE api_keys(id uuid PRIMARY KEY, workspace_id uuid NOT NULL REFERENCES workspaces(id), issued_to_user_id uuid REFERENCES users(id), service_account_id uuid, name text NOT NULL, secret_hash bytea NOT NULL CHECK(octet_length(secret_hash)=32), governance_key_id uuid NOT NULL, created_at timestamptz NOT NULL DEFAULT now(), expires_at timestamptz, revoked_at timestamptz, UNIQUE(workspace_id,id), CHECK((issued_to_user_id IS NOT NULL)<>(service_account_id IS NOT NULL)), FOREIGN KEY(workspace_id,service_account_id) REFERENCES service_accounts(workspace_id,id), FOREIGN KEY(workspace_id,governance_key_id) REFERENCES api_keys(workspace_id,id));
CREATE FUNCTION initialize_key_lineage() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN NEW.governance_key_id:=coalesce(NEW.governance_key_id,NEW.id); RETURN NEW; END $$;
CREATE TRIGGER api_key_lineage_default BEFORE INSERT ON api_keys FOR EACH ROW EXECUTE FUNCTION initialize_key_lineage();
CREATE INDEX api_key_governance_lineage ON api_keys(workspace_id,governance_key_id);
CREATE TABLE provider_connections(id uuid PRIMARY KEY, name text NOT NULL, provider text NOT NULL, credential_ref text NOT NULL CHECK(credential_ref IN ('none','aws:default') OR credential_ref ~ '^env:[A-Z_][A-Z0-9_]*$'), endpoint text, region text, enabled boolean NOT NULL DEFAULT false, created_at timestamptz NOT NULL DEFAULT now(), CHECK(credential_ref<>'none' OR provider IN ('vllm','sglang','ollama','openai_compatible')), CHECK((provider='bedrock')=(credential_ref='aws:default')));
CREATE FUNCTION valid_model_protocols(protocols text[]) RETURNS boolean LANGUAGE sql IMMUTABLE AS $$ SELECT cardinality(protocols) BETWEEN 1 AND 4 AND array_position(protocols,NULL) IS NULL AND protocols <@ ARRAY['chat_completions','responses','messages','embeddings']::text[] AND cardinality(protocols)=(SELECT count(DISTINCT p) FROM unnest(protocols) p) $$;
CREATE TABLE models(id uuid PRIMARY KEY, public_name text NOT NULL UNIQUE, display_name text NOT NULL DEFAULT '', description text, supported_protocols text[] NOT NULL DEFAULT ARRAY['chat_completions'] CHECK(valid_model_protocols(supported_protocols)), enabled boolean NOT NULL DEFAULT true, created_at timestamptz NOT NULL DEFAULT now());
CREATE TABLE deployments(id uuid PRIMARY KEY, model_id uuid NOT NULL REFERENCES models(id), provider_connection_id uuid NOT NULL REFERENCES provider_connections(id), upstream_model text NOT NULL, enabled boolean NOT NULL DEFAULT false, created_at timestamptz NOT NULL DEFAULT now());
CREATE TABLE catalogs(id uuid PRIMARY KEY, name text NOT NULL, description text, created_at timestamptz NOT NULL DEFAULT now());
CREATE TABLE catalog_models(catalog_id uuid NOT NULL REFERENCES catalogs(id), model_id uuid NOT NULL REFERENCES models(id), PRIMARY KEY(catalog_id,model_id));
CREATE TABLE workspace_type_catalogs(kind text NOT NULL CHECK(kind IN ('personal','team','project')), catalog_id uuid NOT NULL REFERENCES catalogs(id), PRIMARY KEY(kind,catalog_id));
CREATE TABLE workspace_catalog_overrides(workspace_id uuid PRIMARY KEY REFERENCES workspaces(id));
CREATE TABLE workspace_catalog_override_items(workspace_id uuid NOT NULL REFERENCES workspace_catalog_overrides(workspace_id) ON DELETE CASCADE, catalog_id uuid NOT NULL REFERENCES catalogs(id), PRIMARY KEY(workspace_id,catalog_id));
CREATE TABLE workspace_model_grants(workspace_id uuid NOT NULL REFERENCES workspaces(id), model_id uuid NOT NULL REFERENCES models(id), source text NOT NULL CHECK(source IN ('catalog','direct')), created_at timestamptz NOT NULL DEFAULT now(), PRIMARY KEY(workspace_id,model_id,source));
CREATE FUNCTION workspace_model_allowed(ws uuid, model uuid) RETURNS boolean LANGUAGE sql STABLE AS $$
 SELECT EXISTS (
   SELECT 1 FROM workspaces w JOIN models m ON m.id=model
   JOIN workspace_model_grants g ON g.workspace_id=w.id AND g.model_id=m.id
   WHERE w.id=ws AND w.disabled_at IS NULL AND m.enabled AND (
     g.source='direct' OR EXISTS (
       SELECT 1 FROM catalog_models cm WHERE cm.model_id=m.id AND (
         EXISTS (SELECT 1 FROM workspace_catalog_override_items i WHERE i.workspace_id=w.id AND i.catalog_id=cm.catalog_id)
         OR (NOT EXISTS (SELECT 1 FROM workspace_catalog_overrides o WHERE o.workspace_id=w.id)
             AND EXISTS (SELECT 1 FROM workspace_type_catalogs t WHERE t.kind=w.kind AND t.catalog_id=cm.catalog_id))
       )
     )
   )
 )
$$;
CREATE TABLE key_model_restrictions(workspace_id uuid NOT NULL, governance_key_id uuid NOT NULL, PRIMARY KEY(workspace_id,governance_key_id), FOREIGN KEY(workspace_id,governance_key_id) REFERENCES api_keys(workspace_id,id));
CREATE TABLE key_model_selections(workspace_id uuid NOT NULL, governance_key_id uuid NOT NULL, model_id uuid NOT NULL REFERENCES models(id), PRIMARY KEY(workspace_id,governance_key_id,model_id), FOREIGN KEY(workspace_id,governance_key_id) REFERENCES key_model_restrictions(workspace_id,governance_key_id) ON DELETE CASCADE);
CREATE TABLE deployment_prices(id uuid PRIMARY KEY, deployment_id uuid NOT NULL REFERENCES deployments(id), input_microusd_per_million bigint NOT NULL CHECK(input_microusd_per_million>=0), output_microusd_per_million bigint NOT NULL CHECK(output_microusd_per_million>=0), input_token_limit bigint NOT NULL CHECK(input_token_limit>0), output_token_limit bigint NOT NULL CHECK(output_token_limit>=0), pricing_version smallint NOT NULL DEFAULT 2 CHECK(pricing_version IN (1,2)), cache_pricing jsonb, created_at timestamptz NOT NULL DEFAULT clock_timestamp(), UNIQUE(deployment_id,id));
CREATE INDEX deployment_price_latest ON deployment_prices(deployment_id,created_at DESC,id DESC);
CREATE TABLE installation_policy(singleton boolean PRIMARY KEY DEFAULT true CHECK(singleton), requests_per_minute bigint CHECK(requests_per_minute>0), tokens_per_minute bigint CHECK(tokens_per_minute>0), concurrent_requests bigint CHECK(concurrent_requests>0), monthly_budget_microusd bigint CHECK(monthly_budget_microusd>=0));
CREATE TABLE workspace_type_policies(kind text PRIMARY KEY CHECK(kind IN ('personal','team','project')), requests_per_minute bigint CHECK(requests_per_minute>0), tokens_per_minute bigint CHECK(tokens_per_minute>0), concurrent_requests bigint CHECK(concurrent_requests>0), monthly_budget_microusd bigint CHECK(monthly_budget_microusd>=0));
CREATE TABLE workspace_platform_policy_overrides(workspace_id uuid PRIMARY KEY REFERENCES workspaces(id), requests_per_minute bigint CHECK(requests_per_minute>0), tokens_per_minute bigint CHECK(tokens_per_minute>0), concurrent_requests bigint CHECK(concurrent_requests>0), monthly_budget_microusd bigint CHECK(monthly_budget_microusd>=0));
CREATE TABLE workspace_local_policies(workspace_id uuid PRIMARY KEY REFERENCES workspaces(id), requests_per_minute bigint CHECK(requests_per_minute>0), tokens_per_minute bigint CHECK(tokens_per_minute>0), concurrent_requests bigint CHECK(concurrent_requests>0), monthly_budget_microusd bigint CHECK(monthly_budget_microusd>=0));
CREATE TABLE key_policies(workspace_id uuid NOT NULL, governance_key_id uuid NOT NULL, requests_per_minute bigint CHECK(requests_per_minute>0), tokens_per_minute bigint CHECK(tokens_per_minute>0), concurrent_requests bigint CHECK(concurrent_requests>0), monthly_budget_microusd bigint CHECK(monthly_budget_microusd>=0), PRIMARY KEY(workspace_id,governance_key_id), FOREIGN KEY(workspace_id,governance_key_id) REFERENCES api_keys(workspace_id,id));
CREATE TABLE inference_executions(id uuid PRIMARY KEY, workspace_id uuid NOT NULL REFERENCES workspaces(id), api_key_id uuid NOT NULL, deployment_id uuid NOT NULL REFERENCES deployments(id), public_model text NOT NULL, provider text NOT NULL, streamed boolean NOT NULL, state text NOT NULL CHECK(state IN ('started','succeeded','failed','cancelled','indeterminate')), error_code text, input_tokens bigint CHECK(input_tokens>=0), output_tokens bigint CHECK(output_tokens>=0), billing_usage jsonb, workload_kind text NOT NULL DEFAULT 'generation' CHECK(workload_kind IN ('generation','embeddings')), elapsed_ms bigint CHECK(elapsed_ms>=0), started_at timestamptz NOT NULL DEFAULT clock_timestamp(), completed_at timestamptz, root_request_id uuid NOT NULL, attempt_number integer NOT NULL DEFAULT 1 CHECK(attempt_number BETWEEN 1 AND 3), details_redacted_at timestamptz, cost_center_id uuid REFERENCES cost_centers(id), cost_center_name text, cost_center_code text, UNIQUE(root_request_id,attempt_number), UNIQUE(workspace_id,api_key_id,deployment_id,id), FOREIGN KEY(workspace_id,api_key_id) REFERENCES api_keys(workspace_id,id));
CREATE INDEX executions_workspace_time ON inference_executions(workspace_id,started_at,id);
CREATE TABLE governance_reservations(execution_id uuid PRIMARY KEY, workspace_id uuid NOT NULL, api_key_id uuid NOT NULL, deployment_id uuid NOT NULL, price_id uuid, admitted_at timestamptz NOT NULL, minute_start timestamptz NOT NULL, month_start timestamptz NOT NULL, lease_expires_at timestamptz NOT NULL, state text NOT NULL CHECK(state IN ('pending','unknown','settled')), reserved_tokens bigint CHECK(reserved_tokens>=0), held_microusd bigint CHECK(held_microusd>=0), actual_microusd bigint CHECK(actual_microusd>=0), input_tokens bigint CHECK(input_tokens>=0), output_tokens bigint CHECK(output_tokens>=0), billing_usage jsonb, cost_components jsonb, unbounded_cost boolean NOT NULL DEFAULT false, FOREIGN KEY(workspace_id,api_key_id,deployment_id,execution_id) REFERENCES inference_executions(workspace_id,api_key_id,deployment_id,id), FOREIGN KEY(deployment_id,price_id) REFERENCES deployment_prices(deployment_id,id));
CREATE INDEX governance_minute ON governance_reservations(minute_start,workspace_id);
CREATE INDEX governance_month ON governance_reservations(month_start,workspace_id);
CREATE INDEX governance_leases ON governance_reservations(lease_expires_at) WHERE state='pending';
CREATE TABLE monetary_ledger(id uuid PRIMARY KEY, execution_id uuid NOT NULL REFERENCES governance_reservations(execution_id), kind text NOT NULL CHECK(kind IN ('hold','settlement','unknown','reconciliation')), amount_microusd bigint CHECK(amount_microusd>=0), input_tokens bigint CHECK(input_tokens>=0), output_tokens bigint CHECK(output_tokens>=0), evidence text CHECK(length(evidence) BETWEEN 1 AND 200), billing_usage jsonb, cost_components jsonb, created_at timestamptz NOT NULL DEFAULT clock_timestamp(), UNIQUE(execution_id,kind));
CREATE TABLE routing_policies(model_id uuid PRIMARY KEY REFERENCES models(id), strategy text NOT NULL DEFAULT 'priority' CHECK(strategy IN ('priority','weighted')), max_attempts integer NOT NULL DEFAULT 1 CHECK(max_attempts BETWEEN 1 AND 3), allow_ambiguous_failover boolean NOT NULL DEFAULT false, required_residency text, created_at timestamptz NOT NULL DEFAULT now());
CREATE TABLE deployment_routing(deployment_id uuid PRIMARY KEY REFERENCES deployments(id), priority integer NOT NULL DEFAULT 0, weight integer NOT NULL DEFAULT 1 CHECK(weight BETWEEN 1 AND 1000), residency text, failure_threshold integer NOT NULL DEFAULT 3 CHECK(failure_threshold>0), cooldown_seconds integer NOT NULL DEFAULT 30 CHECK(cooldown_seconds BETWEEN 1 AND 3600));
CREATE TABLE deployment_health(deployment_id uuid PRIMARY KEY REFERENCES deployments(id), consecutive_failures integer NOT NULL DEFAULT 0 CHECK(consecutive_failures>=0), last_observed_at timestamptz NOT NULL DEFAULT now(), open_until timestamptz);
CREATE TABLE audit_events(id uuid PRIMARY KEY, actor_user_id uuid REFERENCES users(id), workspace_id uuid REFERENCES workspaces(id), action text NOT NULL, resource_type text NOT NULL, resource_id uuid, metadata jsonb NOT NULL DEFAULT '{}', created_at timestamptz NOT NULL DEFAULT now());
CREATE INDEX audit_workspace_time ON audit_events(workspace_id,created_at,id);
CREATE FUNCTION immutable_history() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'immutable history'; END $$;
CREATE TRIGGER deployment_prices_immutable BEFORE UPDATE OR DELETE ON deployment_prices FOR EACH ROW EXECUTE FUNCTION immutable_history();
CREATE TRIGGER deployment_prices_no_truncate BEFORE TRUNCATE ON deployment_prices FOR EACH STATEMENT EXECUTE FUNCTION immutable_history();
CREATE TRIGGER monetary_ledger_immutable BEFORE UPDATE OR DELETE ON monetary_ledger FOR EACH ROW EXECUTE FUNCTION immutable_history();
CREATE TRIGGER monetary_ledger_no_truncate BEFORE TRUNCATE ON monetary_ledger FOR EACH STATEMENT EXECUTE FUNCTION immutable_history();
CREATE TRIGGER audit_events_immutable BEFORE UPDATE OR DELETE ON audit_events FOR EACH ROW EXECUTE FUNCTION immutable_history();
CREATE TRIGGER audit_events_no_truncate BEFORE TRUNCATE ON audit_events FOR EACH STATEMENT EXECUTE FUNCTION immutable_history();

-- Match the Rust billing presence/partition contract. Missing is not zero.
CREATE FUNCTION valid_i64_string(v jsonb, nullable boolean DEFAULT false) RETURNS boolean LANGUAGE plpgsql IMMUTABLE AS $$
BEGIN
 IF v IS NULL OR v='null'::jsonb THEN RETURN nullable; END IF;
 IF jsonb_typeof(v)<>'string' OR length(v #>> '{}') NOT BETWEEN 1 AND 19 OR (v #>> '{}') !~ '^[0-9]+$' THEN RETURN false; END IF;
 RETURN (v #>> '{}')::numeric<=9223372036854775807;
END $$;
CREATE FUNCTION valid_cache_pricing(v jsonb) RETURNS boolean LANGUAGE plpgsql IMMUTABLE AS $$
DECLARE k text; r jsonb; n integer;
BEGIN
 IF v IS NULL OR jsonb_typeof(v)<>'object' THEN RETURN false; END IF;
 SELECT count(*) INTO n FROM jsonb_object_keys(v); IF n<>4 OR NOT(v ?& ARRAY['read','write','write_5m','write_1h']) THEN RETURN false; END IF;
 FOREACH k IN ARRAY ARRAY['read','write','write_5m','write_1h'] LOOP
  r:=v->k; IF jsonb_typeof(r)<>'object' THEN RETURN false; END IF;
  SELECT count(*) INTO n FROM jsonb_object_keys(r);
  IF r->>'status'='priced' THEN
   IF n<>2 OR NOT valid_i64_string(r->'microusd_per_million') THEN RETURN false; END IF;
  ELSIF r->>'status' IN ('unknown','not_applicable') THEN
   IF n<>1 THEN RETURN false; END IF;
  ELSE RETURN false;
  END IF;
 END LOOP;
 RETURN true;
END $$;
CREATE FUNCTION valid_billing_usage(v jsonb) RETURNS boolean LANGUAGE plpgsql IMMUTABLE AS $$
DECLARE k text; parts numeric; aggregate numeric; total numeric; input_floor numeric; n integer;
BEGIN
 IF v IS NULL THEN RETURN true; END IF;
 IF jsonb_typeof(v)<>'object' THEN RETURN false; END IF;
 SELECT count(*) INTO n FROM jsonb_object_keys(v); IF n<>7 THEN RETURN false; END IF;
 FOR k IN SELECT jsonb_object_keys(v) LOOP
  IF NOT(k=ANY(ARRAY['total_input_tokens','uncached_input_tokens','cache_read_input_tokens','cache_write_input_tokens','cache_write_default_input_tokens','cache_write_5m_input_tokens','cache_write_1h_input_tokens'])) OR NOT valid_i64_string(v->k,true) THEN RETURN false; END IF;
 END LOOP;
 parts:=coalesce((v->>'cache_write_default_input_tokens')::numeric,0)+coalesce((v->>'cache_write_5m_input_tokens')::numeric,0)+coalesce((v->>'cache_write_1h_input_tokens')::numeric,0);
 aggregate:=(v->>'cache_write_input_tokens')::numeric;
 IF parts>9223372036854775807 OR (aggregate IS NOT NULL AND (parts>aggregate OR ((v->>'cache_write_default_input_tokens') IS NOT NULL AND (v->>'cache_write_5m_input_tokens') IS NOT NULL AND (v->>'cache_write_1h_input_tokens') IS NOT NULL AND parts<>aggregate))) THEN RETURN false; END IF;
 input_floor:=coalesce((v->>'uncached_input_tokens')::numeric,0)+coalesce((v->>'cache_read_input_tokens')::numeric,0)+greatest(parts,coalesce(aggregate,0));
 total:=(v->>'total_input_tokens')::numeric;
 IF input_floor>9223372036854775807 OR (total IS NOT NULL AND (input_floor>total OR ((v->>'uncached_input_tokens') IS NOT NULL AND (v->>'cache_read_input_tokens') IS NOT NULL AND aggregate IS NOT NULL AND input_floor<>total))) THEN RETURN false; END IF;
 RETURN true;
END $$;
CREATE FUNCTION valid_cost_components(v jsonb) RETURNS boolean LANGUAGE plpgsql IMMUTABLE AS $$
DECLARE k text; n integer; amount numeric:=0;
BEGIN
 IF v IS NULL THEN RETURN true; END IF;
 IF jsonb_typeof(v)<>'object' THEN RETURN false; END IF;
 SELECT count(*) INTO n FROM jsonb_object_keys(v); IF n<>6 THEN RETURN false; END IF;
 FOREACH k IN ARRAY ARRAY['uncached_input_microusd','cache_read_microusd','cache_write_default_microusd','cache_write_5m_microusd','cache_write_1h_microusd','output_microusd'] LOOP
  IF NOT valid_i64_string(v->k) THEN RETURN false; END IF;
  amount:=amount+(v->>k)::numeric;
 END LOOP;
 RETURN amount<=9223372036854775807;
END $$;
CREATE FUNCTION components_total(v jsonb) RETURNS numeric LANGUAGE sql IMMUTABLE AS $$ SELECT sum((value #>> '{}')::numeric) FROM jsonb_each(v) $$;
ALTER TABLE deployment_prices ADD CHECK((pricing_version=1 AND cache_pricing IS NULL) OR (pricing_version=2 AND valid_cache_pricing(cache_pricing)));
ALTER TABLE inference_executions ADD CHECK(valid_billing_usage(billing_usage)), ADD CHECK(coalesce(input_tokens,0)::numeric+coalesce(output_tokens,0)::numeric<=9223372036854775807);
ALTER TABLE governance_reservations ADD CHECK(valid_billing_usage(billing_usage)), ADD CHECK(valid_cost_components(cost_components)), ADD CHECK(cost_components IS NULL OR (actual_microusd IS NOT NULL AND components_total(cost_components)=actual_microusd)), ADD CHECK((state='settled' AND actual_microusd IS NOT NULL AND input_tokens IS NOT NULL AND output_tokens IS NOT NULL) OR (state<>'settled' AND actual_microusd IS NULL));
ALTER TABLE monetary_ledger ADD CHECK(valid_billing_usage(billing_usage)), ADD CHECK(valid_cost_components(cost_components)), ADD CHECK(cost_components IS NULL OR (amount_microusd IS NOT NULL AND components_total(cost_components)=amount_microusd));
