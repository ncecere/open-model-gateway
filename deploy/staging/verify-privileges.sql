-- Enterprise ACL assertions, followed by executable rollback-only probes.
DO $$ DECLARE r record; t text; BEGIN
 SELECT * INTO STRICT r FROM pg_roles WHERE rolname='gateway_runtime';
 IF r.rolsuper OR r.rolcreatedb OR r.rolcreaterole OR r.rolreplication OR r.rolbypassrls THEN RAISE EXCEPTION 'privileged runtime attributes'; END IF;
 IF EXISTS(SELECT FROM pg_auth_members WHERE member=r.oid) THEN RAISE EXCEPTION 'runtime role memberships'; END IF;
 IF EXISTS(SELECT FROM pg_database WHERE datname=current_database() AND datdba=r.oid) OR EXISTS(SELECT FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname='public' AND c.relowner=r.oid) THEN RAISE EXCEPTION 'runtime owns objects'; END IF;
 IF has_database_privilege('gateway_runtime','postgres','CONNECT') OR has_database_privilege('gateway_runtime','template1','CONNECT') THEN RAISE EXCEPTION 'maintenance database access'; END IF;
 IF has_schema_privilege('gateway_runtime','public','CREATE') OR has_database_privilege('gateway_runtime',current_database(),'CREATE') OR has_database_privilege('gateway_runtime',current_database(),'TEMP') THEN RAISE EXCEPTION 'runtime can create objects'; END IF;
 FOREACH t IN ARRAY ARRAY['_sqlx_migrations','deployment_prices','monetary_ledger','audit_events'] LOOP
  IF has_table_privilege('gateway_runtime','public.'||t,'UPDATE,DELETE,TRUNCATE') OR has_any_column_privilege('gateway_runtime','public.'||t,'UPDATE') THEN RAISE EXCEPTION 'mutable immutable history: %',t; END IF;
 END LOOP;
 IF has_table_privilege('gateway_runtime','public._sqlx_migrations','INSERT') OR has_table_privilege('gateway_runtime','public.installation','INSERT,DELETE,TRUNCATE') OR has_column_privilege('gateway_runtime','public.installation','schema_family','UPDATE') OR has_column_privilege('gateway_runtime','public.installation','id','UPDATE') THEN RAISE EXCEPTION 'runtime can rewrite lineage'; END IF;
 FOREACH t IN ARRAY ARRAY['price_lines','max_units','pricing_version','cache_pricing'] LOOP
  IF has_column_privilege('gateway_runtime','public.deployment_prices',t,'UPDATE') THEN RAISE EXCEPTION 'mutable price column: %',t; END IF;
 END LOOP;
 FOREACH t IN ARRAY ARRAY['meter_usage','output_image_variant','provider_cost_microusd'] LOOP
  IF has_column_privilege('gateway_runtime','public.monetary_ledger',t,'UPDATE') THEN RAISE EXCEPTION 'mutable ledger evidence: %',t; END IF;
 END LOOP;
 IF has_column_privilege('gateway_runtime','public.api_keys','governance_key_id','UPDATE') THEN RAISE EXCEPTION 'mutable credential budget lineage'; END IF;
 IF has_any_column_privilege('gateway_runtime','public.policy_budgets','UPDATE') OR has_table_privilege('gateway_runtime','public.policy_budgets','TRUNCATE') THEN RAISE EXCEPTION 'budget rows rewritable in place'; END IF;
 IF has_table_privilege('gateway_runtime','public.key_model_restrictions','UPDATE,DELETE,TRUNCATE') OR has_any_column_privilege('gateway_runtime','public.key_model_restrictions','UPDATE') OR has_any_column_privilege('gateway_runtime','public.key_model_selections','UPDATE') THEN RAISE EXCEPTION 'mutable key restriction provenance'; END IF;
 IF has_table_privilege('gateway_runtime','public.installation_settings','INSERT,DELETE,TRUNCATE') OR has_column_privilege('gateway_runtime','public.installation_settings','singleton','UPDATE') THEN RAISE EXCEPTION 'installation settings row replaceable'; END IF;
 -- SCIM (0014): user links are never removed or re-keyed; group identity fixed; one state row.
 IF has_table_privilege('gateway_runtime','public.scim_users','DELETE,TRUNCATE') OR has_column_privilege('gateway_runtime','public.scim_users','user_id','UPDATE') OR has_column_privilege('gateway_runtime','public.scim_users','created_at','UPDATE') THEN RAISE EXCEPTION 'scim user link removable or re-keyable'; END IF;
 IF has_column_privilege('gateway_runtime','public.scim_groups','id','UPDATE') OR has_any_column_privilege('gateway_runtime','public.scim_group_members','UPDATE') OR has_table_privilege('gateway_runtime','public.scim_groups','TRUNCATE') OR has_table_privilege('gateway_runtime','public.scim_group_members','TRUNCATE') THEN RAISE EXCEPTION 'scim group identity rewritable'; END IF;
 IF has_table_privilege('gateway_runtime','public.scim_state','INSERT,DELETE,TRUNCATE') OR has_column_privilege('gateway_runtime','public.scim_state','singleton','UPDATE') THEN RAISE EXCEPTION 'scim state row replaceable'; END IF;
 -- Alerts (0011): no removal; incidents resolve once; rule identity/scope fixed.
 FOREACH t IN ARRAY ARRAY['alert_rules','alert_events','alert_deliveries','alert_reads'] LOOP
  IF has_table_privilege('gateway_runtime','public.'||t,'DELETE,TRUNCATE') THEN RAISE EXCEPTION 'alert history removable: %',t; END IF;
 END LOOP;
 IF has_any_column_privilege('gateway_runtime','public.alert_reads','UPDATE') THEN RAISE EXCEPTION 'read marks rewritable'; END IF;
 FOREACH t IN ARRAY ARRAY['id','scope','workspace_id','kind','created_by','created_at'] LOOP
  IF has_column_privilege('gateway_runtime','public.alert_rules',t,'UPDATE') THEN RAISE EXCEPTION 'mutable alert rule identity: %',t; END IF;
 END LOOP;
 FOREACH t IN ARRAY ARRAY['id','rule_id','builtin','kind','subject_key','level','severity','workspace_id','provider_connection_id','summary','details','fired_at'] LOOP
  IF has_column_privilege('gateway_runtime','public.alert_events',t,'UPDATE') THEN RAISE EXCEPTION 'mutable alert incident: %',t; END IF;
 END LOOP;
 IF has_column_privilege('gateway_runtime','public.alert_deliveries','event_id','UPDATE') OR has_column_privilege('gateway_runtime','public.alert_deliveries','transition','UPDATE') THEN RAISE EXCEPTION 'mutable alert delivery identity'; END IF;
 FOREACH t IN ARRAY ARRAY['workload_kind','cost_center_id','cost_center_name','cost_center_code','workspace_id','api_key_id','deployment_id','upstream_model','root_request_id','attempt_number','streamed'] LOOP
  IF has_column_privilege('gateway_runtime','public.inference_executions',t,'UPDATE') THEN RAISE EXCEPTION 'mutable admission attribution: %',t; END IF;
 END LOOP;
 FOR t IN SELECT tablename FROM pg_tables WHERE schemaname='public' AND tablename<>'_sqlx_migrations' LOOP
  IF NOT has_table_privilege('gateway_runtime','public.'||t,'SELECT') THEN RAISE EXCEPTION 'unreviewed table: %',t; END IF;
 END LOOP;
 IF EXISTS(SELECT FROM pg_proc p JOIN pg_namespace n ON n.oid=p.pronamespace WHERE n.nspname='public' AND has_function_privilege('gateway_runtime',p.oid,'EXECUTE') AND p.proname NOT IN ('lock_installation','workspace_model_allowed','valid_model_protocols','valid_i64_string','valid_cache_pricing','valid_billing_usage','valid_cost_components','components_total','valid_meter_usage','valid_meter_variant','valid_price_lines','valid_max_units')) THEN RAISE EXCEPTION 'unexpected executable function'; END IF;
END $$;
BEGIN;
SET LOCAL ROLE gateway_runtime;
SELECT version,checksum,success FROM public._sqlx_migrations ORDER BY version;
SELECT lock_installation();
SELECT id FROM public.users WHERE false FOR UPDATE;
SELECT model_id FROM public.workspace_model_grants WHERE false FOR SHARE;
DO $$ DECLARE
 u uuid:=gen_random_uuid(); ws uuid:=gen_random_uuid(); personal uuid:=gen_random_uuid();
 k uuid:=gen_random_uuid(); m uuid:=gen_random_uuid(); pc uuid:=gen_random_uuid();
 d uuid:=gen_random_uuid(); price uuid:=gen_random_uuid(); e uuid:=gen_random_uuid(); cat uuid:=gen_random_uuid(); changed uuid;
 rates jsonb:='{"read":{"status":"priced","microusd_per_million":"1"},"write":{"status":"priced","microusd_per_million":"2"},"write_5m":{"status":"priced","microusd_per_million":"3"},"write_1h":{"status":"priced","microusd_per_million":"4"}}';
BEGIN
 INSERT INTO users(id,email,oidc_link_allowed) VALUES(u,'rollback-'||u||'@example.invalid',true);
 INSERT INTO platform_role_grants(id,user_id,role,source) VALUES(gen_random_uuid(),u,'user','manual');
 INSERT INTO workspaces(id,name,kind) VALUES(ws,'Rollback project','project');
 INSERT INTO workspaces(id,name,kind,owner_user_id) VALUES(personal,'Rollback personal','personal',u);
 INSERT INTO workspace_membership_grants(id,workspace_id,user_id,role,source) VALUES(gen_random_uuid(),ws,u,'owner','manual'),(gen_random_uuid(),personal,u,'owner','manual');
 IF NOT EXISTS(SELECT FROM effective_platform_roles WHERE user_id=u AND role='user') OR NOT EXISTS(SELECT FROM effective_workspace_memberships WHERE user_id=u AND workspace_id=ws AND role='owner') THEN RAISE EXCEPTION 'live grant projections failed'; END IF;
 -- Exact sign-in upsert needs the owner-preserving UPDATE privilege.
 INSERT INTO workspaces(id,name,kind,owner_user_id) VALUES(gen_random_uuid(),'Personal','personal',u) ON CONFLICT(owner_user_id) WHERE kind='personal' AND disabled_at IS NULL DO UPDATE SET owner_user_id=EXCLUDED.owner_user_id;
 INSERT INTO api_keys(id,workspace_id,issued_to_user_id,name,secret_hash) VALUES(k,ws,u,'Never issued',decode(repeat('00',32),'hex'));
 IF (SELECT governance_key_id FROM api_keys WHERE id=k)<>k THEN RAISE EXCEPTION 'lineage default failed'; END IF;
 INSERT INTO models(id,public_name,display_name,supported_protocols) VALUES(m,'rollback-'||m,'Rollback',ARRAY['chat_completions','responses']);
 INSERT INTO catalogs(id,name) VALUES(cat,'Rollback catalog');
 INSERT INTO catalog_models(catalog_id,model_id) VALUES(cat,m);
 INSERT INTO workspace_type_catalogs(kind,catalog_id) VALUES('project',cat);
 INSERT INTO workspace_model_grants(workspace_id,model_id,source) VALUES(ws,m,'catalog');
 IF NOT workspace_model_allowed(ws,m) THEN RAISE EXCEPTION 'catalog eligibility failed'; END IF;
 -- Model-scoped catalog replacement (PUT /models/{id}/catalogs, model setup) needs no new privilege.
 PERFORM id FROM models WHERE id=m FOR SHARE;
 DELETE FROM catalog_models WHERE model_id=m AND NOT catalog_id=ANY(ARRAY[cat]) RETURNING catalog_id INTO changed;
 INSERT INTO catalog_models(catalog_id,model_id) SELECT unnest(ARRAY[cat]),m ON CONFLICT DO NOTHING RETURNING catalog_id INTO changed;
 IF changed IS NOT NULL OR NOT workspace_model_allowed(ws,m) THEN RAISE EXCEPTION 'model catalog delta failed'; END IF;
 -- Stacked budgets (0005): one budget per scope and period, replaced per scope.
 INSERT INTO installation_policy(singleton,requests_per_minute) VALUES(true,10) ON CONFLICT(singleton) DO UPDATE SET requests_per_minute=EXCLUDED.requests_per_minute;
 INSERT INTO workspace_type_policies(kind) VALUES('project') ON CONFLICT(kind) DO UPDATE SET tokens_per_minute=EXCLUDED.tokens_per_minute;
 INSERT INTO workspace_platform_policy_overrides(workspace_id) VALUES(ws) ON CONFLICT(workspace_id) DO UPDATE SET concurrent_requests=EXCLUDED.concurrent_requests;
 INSERT INTO workspace_local_policies(workspace_id) VALUES(ws) ON CONFLICT(workspace_id) DO NOTHING;
 INSERT INTO key_policies(workspace_id,governance_key_id) VALUES(ws,k) ON CONFLICT(workspace_id,governance_key_id) DO NOTHING;
 INSERT INTO policy_budgets(layer,period,amount_microusd) VALUES('installation','day',1),('installation','lifetime',9);
 INSERT INTO policy_budgets(layer,kind,period,amount_microusd) VALUES('type','project','week',1);
 INSERT INTO policy_budgets(layer,workspace_id,period,amount_microusd) VALUES('override',ws,'month',1),('local',ws,'day',1),('local',ws,'month',2);
 INSERT INTO policy_budgets(layer,workspace_id,governance_key_id,period,amount_microusd) VALUES('key',ws,k,'lifetime',1);
 DELETE FROM policy_budgets WHERE layer='local' AND workspace_id=ws AND period='day';
 IF (SELECT count(*) FROM policy_budgets WHERE workspace_id=ws)<>3 THEN RAISE EXCEPTION 'stacked budget replacement failed'; END IF;
 BEGIN INSERT INTO policy_budgets(layer,workspace_id,period,amount_microusd) VALUES('local',ws,'month',3); RAISE EXCEPTION 'one budget per scope and period absent'; EXCEPTION WHEN unique_violation THEN NULL; END;
 BEGIN INSERT INTO policy_budgets(layer,period,amount_microusd) VALUES('installation','year',1); RAISE EXCEPTION 'budget period constraint absent'; EXCEPTION WHEN check_violation THEN NULL; END;
 BEGIN UPDATE policy_budgets SET amount_microusd=99 WHERE workspace_id=ws; RAISE EXCEPTION 'budget rows rewritable in place'; EXCEPTION WHEN insufficient_privilege THEN NULL; END;
 -- Reversible key disablement; revocation stays a separate column.
 UPDATE api_keys SET disabled_at=now() WHERE id=k;
 UPDATE api_keys SET disabled_at=NULL WHERE id=k;
 PERFORM max(started_at) FROM inference_executions WHERE api_key_id=k;
 PERFORM admitted_at FROM governance_reservations WHERE workspace_id=ws AND admitted_at>=date_trunc('week',now(),'UTC');
 INSERT INTO key_model_restrictions(workspace_id,governance_key_id) VALUES(ws,k);
 INSERT INTO key_model_selections(workspace_id,governance_key_id,model_id) VALUES(ws,k,m);
 -- Retire unavailable selections, preserve explicit deny-all header.
 INSERT INTO workspace_catalog_overrides(workspace_id) VALUES(ws);
 DELETE FROM workspace_model_grants WHERE workspace_id=ws AND model_id=m AND source='catalog';
 DELETE FROM key_model_selections WHERE workspace_id=ws AND NOT workspace_model_allowed(ws,model_id);
 IF workspace_model_allowed(ws,m) OR EXISTS(SELECT FROM key_model_selections WHERE workspace_id=ws) OR NOT EXISTS(SELECT FROM key_model_restrictions WHERE workspace_id=ws) THEN RAISE EXCEPTION 'catalog loss did not preserve deny-all'; END IF;
 INSERT INTO workspace_model_grants(workspace_id,model_id,source) VALUES(ws,m,'direct');
 IF NOT workspace_model_allowed(ws,m) OR EXISTS(SELECT FROM key_model_selections WHERE workspace_id=ws) THEN RAISE EXCEPTION 'direct access/key resurrection invariant'; END IF;
 INSERT INTO provider_connections(id,name,provider,credential_ref,endpoint) VALUES(pc,'Rollback local','openai_compatible','none','http://127.0.0.1:19091/v1');
 INSERT INTO deployments(id,model_id,provider_connection_id,upstream_model) VALUES(d,m,pc,'disabled-probe');
 -- Bedrock identity modes (0008): reference shapes, identity/endpoint replacement, immutable region.
 INSERT INTO provider_connections(id,name,provider,credential_ref,region) VALUES(gen_random_uuid(),'Rollback Bedrock','bedrock','aws:role:arn:aws:iam::123456789012:role/gateway/bedrock;external_id=probe-external;session_name=gateway-probe','us-east-1');
 UPDATE provider_connections SET credential_ref='aws:profile:bedrock-prod',endpoint='https://vpce-0probe.bedrock-runtime.us-east-1.vpce.amazonaws.com' WHERE name='Rollback Bedrock';
 UPDATE provider_connections SET credential_ref='aws:default',endpoint=NULL WHERE name='Rollback Bedrock';
 BEGIN UPDATE provider_connections SET credential_ref='aws:role:not-an-arn' WHERE name='Rollback Bedrock'; RAISE EXCEPTION 'aws reference shape constraint absent'; EXCEPTION WHEN check_violation THEN NULL; END;
 BEGIN UPDATE provider_connections SET endpoint='http://bedrock.internal' WHERE name='Rollback Bedrock'; RAISE EXCEPTION 'bedrock https endpoint constraint absent'; EXCEPTION WHEN check_violation THEN NULL; END;
 BEGIN UPDATE provider_connections SET credential_ref='aws:default' WHERE id=pc; RAISE EXCEPTION 'aws reference outside bedrock allowed'; EXCEPTION WHEN check_violation THEN NULL; END;
 BEGIN UPDATE provider_connections SET region='eu-west-1' WHERE name='Rollback Bedrock'; RAISE EXCEPTION 'connection region rewrite allowed'; EXCEPTION WHEN insufficient_privilege THEN NULL; END;
 INSERT INTO deployment_prices(id,deployment_id,input_microusd_per_million,output_microusd_per_million,input_token_limit,output_token_limit,pricing_version,cache_pricing) VALUES(price,d,1,1,100,10,2,rates);
 -- Pricing v3: immutable price lines and per-meter ceilings.
 INSERT INTO deployment_prices(id,deployment_id,input_token_limit,output_token_limit,pricing_version,price_lines,max_units) VALUES(gen_random_uuid(),d,100,10,3,'[{"meter":"input_tokens","microusd_per_batch":"100000","batch":1000000,"unit_label":"/M tokens","sku_label":"Input"},{"meter":"output_images","microusd_per_batch":"20500","batch":1,"unit_label":"/image","sku_label":"Image","variant":"768"},{"meter":"search_units","not_applicable":true}]','{"output_images":"4"}');
 INSERT INTO inference_executions(id,workspace_id,api_key_id,deployment_id,public_model,provider,streamed,state,root_request_id) VALUES(e,ws,k,d,'rollback','openai_compatible',false,'started',e);
 INSERT INTO governance_reservations(execution_id,workspace_id,api_key_id,deployment_id,price_id,admitted_at,minute_start,month_start,lease_expires_at,state,reserved_tokens,held_microusd) VALUES(e,ws,k,d,price,now(),date_trunc('minute',now()),date_trunc('month',now()),now()+interval '1 minute','pending',110,1);
 INSERT INTO monetary_ledger(id,execution_id,kind,amount_microusd) VALUES(gen_random_uuid(),e,'hold',1);
 -- Settlement writes meter evidence on executions/reservations and appends it to the ledger.
 UPDATE inference_executions SET state='succeeded',meter_usage='{"output_images":"1","input_characters":null,"input_audio_seconds_ms":null,"output_audio_seconds_ms":null,"search_units":null,"requests":"1"}',output_image_variant='768',provider_cost_microusd=20500 WHERE id=e;
 UPDATE governance_reservations SET state='unknown',meter_usage=(SELECT meter_usage FROM inference_executions WHERE id=e),output_image_variant='768',provider_cost_microusd=20500 WHERE execution_id=e;
 INSERT INTO monetary_ledger(id,execution_id,kind,amount_microusd,meter_usage,output_image_variant,provider_cost_microusd) SELECT gen_random_uuid(),e,'unknown',1,meter_usage,output_image_variant,provider_cost_microusd FROM inference_executions WHERE id=e;
 -- Request telemetry (0009): admission snapshot + labels, finish telemetry, retention clearing.
 INSERT INTO inference_executions(id,workspace_id,api_key_id,deployment_id,public_model,provider,streamed,state,root_request_id,attempt_number,upstream_model,client_session_id,client_app) VALUES(gen_random_uuid(),ws,k,d,'rollback','openai_compatible',true,'started',e,2,'disabled-probe','probe session','Probe app');
 UPDATE inference_executions SET finish_reason='stop',time_to_first_token_ms=1,generation_ms=2,reasoning_tokens=0 WHERE root_request_id=e;
 UPDATE inference_executions SET client_session_id=NULL,client_app=NULL,details_redacted_at=now() WHERE root_request_id=e AND client_session_id IS NOT NULL;
 PERFORM count(*) FROM inference_executions WHERE workspace_id=ws AND client_session_id='probe session';
 BEGIN UPDATE inference_executions SET upstream_model='rewrite' WHERE id=e; RAISE EXCEPTION 'upstream snapshot rewrite allowed'; EXCEPTION WHEN insufficient_privilege THEN NULL; END;
 BEGIN UPDATE inference_executions SET finish_reason='bogus' WHERE id=e; RAISE EXCEPTION 'finish reason constraint absent'; EXCEPTION WHEN check_violation THEN NULL; END;
 BEGIN UPDATE inference_executions SET client_session_id=' padded' WHERE id=e; RAISE EXCEPTION 'session label constraint absent'; EXCEPTION WHEN check_violation THEN NULL; END;
 -- Reported upstream model (0013): written at finish (and on insert), bounded and validated.
 INSERT INTO inference_executions(id,workspace_id,api_key_id,deployment_id,public_model,provider,streamed,state,root_request_id,attempt_number,upstream_model,reported_upstream_model) VALUES(gen_random_uuid(),ws,k,d,'rollback','openai_compatible',false,'started',e,3,'disabled-probe','probe-served-model');
 UPDATE inference_executions SET reported_upstream_model='openai/gpt-probe-2026-01-01' WHERE root_request_id=e;
 PERFORM count(*) FROM inference_executions WHERE root_request_id=e AND coalesce(reported_upstream_model,upstream_model) IS NOT NULL;
 BEGIN UPDATE inference_executions SET reported_upstream_model='has space' WHERE id=e; RAISE EXCEPTION 'reported model constraint absent'; EXCEPTION WHEN check_violation THEN NULL; END;
 BEGIN UPDATE inference_executions SET reported_upstream_model=repeat('m',257) WHERE id=e; RAISE EXCEPTION 'reported model bound absent'; EXCEPTION WHEN check_violation THEN NULL; END;
 -- Platform overview aggregates are read-only over already-granted relations.
 PERFORM (SELECT count(*) FROM effective_platform_roles),(SELECT count(*) FROM oidc_group_mappings WHERE enabled),(SELECT count(*) FILTER(WHERE kind='team') FROM workspace_type_catalogs),(SELECT count(e2.id)::text||coalesce(sum(r.actual_microusd),0)::text FROM inference_executions e2 LEFT JOIN governance_reservations r ON r.execution_id=e2.id WHERE e2.started_at>=statement_timestamp()-interval '7 days');
 INSERT INTO audit_events(id,actor_user_id,workspace_id,action,resource_type) VALUES(gen_random_uuid(),u,ws,'staging.rollback_probe','workspace');
 BEGIN INSERT INTO service_accounts(id,workspace_id,name) VALUES(gen_random_uuid(),personal,'Forbidden personal service'); RAISE EXCEPTION 'shared-resource constraint absent'; EXCEPTION WHEN check_violation THEN NULL; END;
 BEGIN INSERT INTO deployment_prices(id,deployment_id,input_microusd_per_million,output_microusd_per_million,input_token_limit,output_token_limit,pricing_version,cache_pricing) VALUES(gen_random_uuid(),d,1,1,100,10,2,'{}'); RAISE EXCEPTION 'cache pricing shape constraint absent'; EXCEPTION WHEN check_violation THEN NULL; END;
 BEGIN INSERT INTO models(id,public_name,supported_protocols) VALUES(gen_random_uuid(),'bad-'||m,ARRAY['embeddings','embeddings']); RAISE EXCEPTION 'capability constraint absent'; EXCEPTION WHEN check_violation THEN NULL; END;
 BEGIN UPDATE deployment_prices SET input_microusd_per_million=99 WHERE id=price; RAISE EXCEPTION 'price mutation allowed'; EXCEPTION WHEN insufficient_privilege THEN NULL; END;
 BEGIN UPDATE deployment_prices SET price_lines='[]' WHERE id=price; RAISE EXCEPTION 'price line mutation allowed'; EXCEPTION WHEN insufficient_privilege THEN NULL; END;
 BEGIN UPDATE monetary_ledger SET meter_usage=NULL WHERE execution_id=e; RAISE EXCEPTION 'ledger evidence mutation allowed'; EXCEPTION WHEN insufficient_privilege THEN NULL; END;
 BEGIN INSERT INTO deployment_prices(id,deployment_id,input_token_limit,output_token_limit,pricing_version,price_lines,max_units) VALUES(gen_random_uuid(),d,100,10,3,'[{"meter":"bogus","not_applicable":true}]','{}'); RAISE EXCEPTION 'price line constraint absent'; EXCEPTION WHEN check_violation THEN NULL; END;
 BEGIN INSERT INTO models(id,public_name,supported_protocols) VALUES(gen_random_uuid(),'mixed-'||m,ARRAY['chat_completions','images']); RAISE EXCEPTION 'workload-group constraint absent'; EXCEPTION WHEN check_violation THEN NULL; END;
 BEGIN DELETE FROM monetary_ledger WHERE execution_id=e; RAISE EXCEPTION 'ledger removal allowed'; EXCEPTION WHEN insufficient_privilege THEN NULL; END;
 BEGIN DELETE FROM audit_events WHERE actor_user_id=u; RAISE EXCEPTION 'audit removal allowed'; EXCEPTION WHEN insufficient_privilege THEN NULL; END;
 BEGIN UPDATE inference_executions SET cost_center_name='rewrite' WHERE id=e; RAISE EXCEPTION 'attribution rewrite allowed'; EXCEPTION WHEN insufficient_privilege THEN NULL; END;
 BEGIN UPDATE installation SET schema_family='enterprise_v1'; RAISE EXCEPTION 'schema-family rewrite allowed'; EXCEPTION WHEN insufficient_privilege THEN NULL; END;
 BEGIN DELETE FROM key_model_restrictions WHERE workspace_id=ws; RAISE EXCEPTION 'deny-all header removal allowed'; EXCEPTION WHEN insufficient_privilege THEN NULL; END;
 BEGIN UPDATE api_keys SET secret_hash=decode(repeat('01',32),'hex') WHERE id=k; RAISE EXCEPTION 'credential rewrite allowed'; EXCEPTION WHEN insufficient_privilege THEN NULL; END;
 BEGIN UPDATE api_keys SET governance_key_id=k WHERE id=k; RAISE EXCEPTION 'credential budget lineage rewrite allowed'; EXCEPTION WHEN insufficient_privilege THEN NULL; END;
 BEGIN TRUNCATE TABLE monetary_ledger; RAISE EXCEPTION 'ledger truncate allowed'; EXCEPTION WHEN insufficient_privilege THEN NULL; END;
 BEGIN CREATE TABLE public.runtime_probe_forbidden(i integer); RAISE EXCEPTION 'runtime DDL allowed'; EXCEPTION WHEN insufficient_privilege THEN NULL; END;
 -- Installation settings (0010): Admin > Settings writes, references only, bounded values.
 PERFORM support_url FROM installation_settings WHERE singleton FOR NO KEY UPDATE;
 UPDATE installation SET name='Rollback installation' WHERE singleton;
 UPDATE installation_settings SET support_url='https://help.example.invalid',logo_url=NULL,human_key_max_lifetime_days=30,openrouter_data_collection='allow',request_log_retention_days=90,smtp_host='127.0.0.1',smtp_port=2525,smtp_tls='none',smtp_username='relay',smtp_password_ref='env:SMTP_PASSWORD',smtp_from_address='gateway@example.invalid',smtp_from_name='Gateway',smtp_last_test_at=now(),smtp_last_test_ok=false,smtp_last_test_error='connection',updated_at=now(),updated_by=u WHERE singleton;
 BEGIN UPDATE installation_settings SET smtp_password_ref='plain-secret' WHERE singleton; RAISE EXCEPTION 'smtp password stored as a value'; EXCEPTION WHEN check_violation THEN NULL; END;
 BEGIN UPDATE installation_settings SET request_log_retention_days=1 WHERE singleton; RAISE EXCEPTION 'retention bound absent'; EXCEPTION WHEN check_violation THEN NULL; END;
 BEGIN UPDATE installation_settings SET logo_url='http://logo.example.invalid/x.png' WHERE singleton; RAISE EXCEPTION 'https logo constraint absent'; EXCEPTION WHEN check_violation THEN NULL; END;
 BEGIN DELETE FROM installation_settings; RAISE EXCEPTION 'settings row removal allowed'; EXCEPTION WHEN insufficient_privilege THEN NULL; END;
 BEGIN INSERT INTO installation_settings(singleton) VALUES(true); RAISE EXCEPTION 'settings row insert allowed'; EXCEPTION WHEN insufficient_privilege THEN NULL; END;
 -- Alerts (0011): rules, incidents resolved once, delivery outcomes, read marks.
 DECLARE ar uuid:=gen_random_uuid(); ev uuid:=gen_random_uuid(); BEGIN
  INSERT INTO alert_rules(id,scope,workspace_id,kind,name,budget_layers,thresholds,notify_workspace_admins,notify_emails,created_by,updated_by) VALUES(ar,'workspace',ws,'budget_threshold','Rollback budgets',ARRAY['local','key'],ARRAY[50,80,100],true,ARRAY['ops@example.invalid'],u,u);
  INSERT INTO alert_rules(id,scope,kind,name,window_minutes,consecutive_failures,provider_connection_id,notify_platform_admins) VALUES(gen_random_uuid(),'installation','provider_failing','Rollback upstream',15,3,pc,true);
  INSERT INTO alert_rules(id,scope,kind,name,spike_factor_percent,min_spend_microusd) VALUES(gen_random_uuid(),'installation','spend_spike','Rollback spike',300,1000000);
  UPDATE alert_rules SET name='Rollback budgets 2',enabled=false,thresholds=ARRAY[90],notify_emails='{}',updated_at=now(),updated_by=u WHERE id=ar;
  PERFORM id FROM alert_rules WHERE id=ar FOR UPDATE;
  INSERT INTO alert_events(id,rule_id,kind,subject_key,level,severity,workspace_id,summary,details) VALUES(ev,ar,'budget_threshold','local:'||ws||':-:month',80,'warning',ws,'Workspace monthly budget reached 80%','{"used_microusd":"8"}');
  INSERT INTO alert_events(id,rule_id,kind,subject_key,level,severity,workspace_id,summary) VALUES(gen_random_uuid(),ar,'budget_threshold','local:'||ws||':-:month',100,'critical',ws,'Duplicate') ON CONFLICT DO NOTHING;
  IF (SELECT count(*) FROM alert_events WHERE rule_id=ar AND resolved_at IS NULL)<>1 THEN RAISE EXCEPTION 'duplicate open alert allowed'; END IF;
  INSERT INTO alert_events(id,builtin,kind,subject_key,level,severity,workspace_id,summary) VALUES(gen_random_uuid(),'personal_budget','budget_threshold','key:'||personal||':-:day',100,'critical',personal,'API key daily budget reached 100%');
  PERFORM id FROM alert_events WHERE id=ev FOR UPDATE;
  INSERT INTO alert_deliveries(id,event_id,transition) VALUES(gen_random_uuid(),ev,'fired');
  UPDATE alert_deliveries SET status='partial',recipients=2,sent=1,failed=1,error='rejected',completed_at=now() WHERE event_id=ev;
  UPDATE alert_events SET resolved_at=now(),resolution='cleared' WHERE id=ev;
  INSERT INTO alert_reads(user_id,event_id) VALUES(u,ev);
  INSERT INTO alert_reads(user_id,event_id) VALUES(u,ev) ON CONFLICT DO NOTHING;
  PERFORM count(*) FROM alert_events e LEFT JOIN alert_rules r ON r.id=e.rule_id LEFT JOIN alert_reads x ON x.event_id=e.id AND x.user_id=u WHERE r.scope='workspace' AND EXISTS(SELECT 1 FROM effective_workspace_memberships m WHERE m.workspace_id=e.workspace_id AND m.user_id=u);
  PERFORM pg_try_advisory_xact_lock(72419507);
  BEGIN UPDATE alert_events SET resolved_at=now(),resolution='cleared' WHERE id=ev; RAISE EXCEPTION 'alert re-resolve allowed'; EXCEPTION WHEN raise_exception THEN IF SQLERRM NOT LIKE 'alert events resolve once%' THEN RAISE; END IF; END;
  BEGIN UPDATE alert_events SET summary='rewrite' WHERE id=ev; RAISE EXCEPTION 'alert summary rewrite allowed'; EXCEPTION WHEN insufficient_privilege THEN NULL; END;
  BEGIN DELETE FROM alert_events WHERE id=ev; RAISE EXCEPTION 'alert removal allowed'; EXCEPTION WHEN insufficient_privilege THEN NULL; END;
  BEGIN DELETE FROM alert_rules WHERE id=ar; RAISE EXCEPTION 'alert rule removal allowed'; EXCEPTION WHEN insufficient_privilege THEN NULL; END;
  BEGIN UPDATE alert_rules SET kind='spend_spike' WHERE id=ar; RAISE EXCEPTION 'alert rule kind rewrite allowed'; EXCEPTION WHEN insufficient_privilege THEN NULL; END;
  BEGIN UPDATE alert_reads SET read_at=now() WHERE user_id=u; RAISE EXCEPTION 'read mark rewrite allowed'; EXCEPTION WHEN insufficient_privilege THEN NULL; END;
  BEGIN INSERT INTO alert_rules(id,scope,workspace_id,kind,name,spike_factor_percent,min_spend_microusd) VALUES(gen_random_uuid(),'workspace',personal,'spend_spike','Personal rule',300,1); RAISE EXCEPTION 'personal alert rule allowed'; EXCEPTION WHEN check_violation THEN NULL; END;
  BEGIN INSERT INTO alert_rules(id,scope,workspace_id,kind,name,window_minutes,consecutive_failures) VALUES(gen_random_uuid(),'workspace',ws,'provider_failing','Workspace upstream',15,3); RAISE EXCEPTION 'workspace connection rule allowed'; EXCEPTION WHEN check_violation THEN NULL; END;
  BEGIN INSERT INTO alert_events(id,rule_id,builtin,kind,subject_key,level,severity,summary) VALUES(gen_random_uuid(),ar,'personal_budget','budget_threshold','x',1,'warning','x'); RAISE EXCEPTION 'ambiguous alert source allowed'; EXCEPTION WHEN check_violation THEN NULL; END;
 END;
 -- SCIM (0014): provisioning upserts, group-provenance sync, deactivation, cleanup.
 DECLARE g uuid:=gen_random_uuid(); mp uuid:=gen_random_uuid(); BEGIN
  INSERT INTO scim_users(user_id,user_name,external_id,given_name,family_name,active) VALUES(u,'rollback-'||u,'ext-'||u,'Roll','Back',true) ON CONFLICT(user_id) DO UPDATE SET user_name=EXCLUDED.user_name,external_id=EXCLUDED.external_id,given_name=EXCLUDED.given_name,family_name=EXCLUDED.family_name,active=EXCLUDED.active,updated_at=now();
  INSERT INTO scim_users(user_id,user_name,active) VALUES(u,'rollback2-'||u,false) ON CONFLICT(user_id) DO UPDATE SET user_name=EXCLUDED.user_name,active=EXCLUDED.active,updated_at=now();
  PERFORM u2.id FROM users u2 LEFT JOIN scim_users s ON s.user_id=u2.id WHERE u2.cleaned_at IS NULL AND lower(coalesce(s.user_name,u2.email))=lower('rollback2-'||u) FOR UPDATE OF u2;
  INSERT INTO oidc_group_mappings(id,issuer,group_value,target_kind,platform_role) VALUES(mp,'https://issuer.example.invalid','Rollback group','platform','user');
  INSERT INTO scim_groups(id,display_name,external_id) VALUES(g,'Rollback group','grp-'||g);
  PERFORM id FROM scim_groups WHERE id=g FOR UPDATE;
  INSERT INTO scim_group_members(group_id,user_id) SELECT g,unnest(ARRAY[u]) ON CONFLICT DO NOTHING;
  PERFORM v FROM scim_group_members m JOIN scim_groups sg ON sg.id=m.group_id CROSS JOIN LATERAL (VALUES (sg.display_name),(sg.external_id)) x(v) WHERE m.user_id=u AND v IS NOT NULL;
  INSERT INTO platform_role_grants(id,user_id,role,source,mapping_id) VALUES(gen_random_uuid(),u,'user','group',mp);
  PERFORM g2.user_id FROM platform_role_grants g2 JOIN oidc_group_mappings m2 ON m2.id=g2.mapping_id WHERE g2.source='group' AND g2.revoked_at IS NULL AND m2.group_value=ANY(ARRAY['Rollback group']);
  UPDATE scim_groups SET display_name='Rollback group 2',external_id=NULL,updated_at=now() WHERE id=g;
  UPDATE platform_role_grants SET revoked_at=now() WHERE user_id=u AND mapping_id=mp AND revoked_at IS NULL;
  UPDATE users SET disabled_at=coalesce(disabled_at,now()),cleanup_due_at=coalesce(cleanup_due_at,now()+interval '30 days'),disable_reason='scim_deactivated' WHERE id=u AND cleaned_at IS NULL;
  UPDATE users SET disabled_at=NULL,cleanup_due_at=NULL,disable_reason=NULL WHERE id=u AND disable_reason='scim_deactivated' AND cleanup_due_at>now();
  UPDATE scim_state SET last_write_at=now() WHERE singleton;
  PERFORM (SELECT count(*) FROM scim_users),(SELECT count(*) FROM scim_group_members),(SELECT last_write_at FROM scim_state WHERE singleton);
  DELETE FROM scim_group_members WHERE group_id=g AND user_id=ANY(ARRAY[u]);
  INSERT INTO scim_group_members(group_id,user_id) VALUES(g,u);
  DELETE FROM scim_groups WHERE id=g;
  IF EXISTS(SELECT FROM scim_group_members WHERE group_id=g) THEN RAISE EXCEPTION 'scim membership cascade failed'; END IF;
  UPDATE scim_users SET user_name=NULL,external_id=NULL,given_name=NULL,family_name=NULL,active=false,updated_at=now() WHERE user_id=u;
  DELETE FROM scim_group_members WHERE user_id=u;
  BEGIN DELETE FROM scim_users WHERE user_id=u; RAISE EXCEPTION 'scim user link removal allowed'; EXCEPTION WHEN insufficient_privilege THEN NULL; END;
  BEGIN UPDATE scim_users SET user_id=gen_random_uuid() WHERE user_id=u; RAISE EXCEPTION 'scim user link re-key allowed'; EXCEPTION WHEN insufficient_privilege THEN NULL; END;
  BEGIN INSERT INTO scim_state(singleton) VALUES(true); RAISE EXCEPTION 'scim state insert allowed'; EXCEPTION WHEN insufficient_privilege THEN NULL; END;
  BEGIN INSERT INTO scim_groups(id,display_name) VALUES(gen_random_uuid(),'bad'||chr(7)); RAISE EXCEPTION 'scim group name constraint absent'; EXCEPTION WHEN check_violation THEN NULL; END;
  BEGIN INSERT INTO scim_users(user_id,user_name) VALUES(gen_random_uuid(),'orphan'); RAISE EXCEPTION 'scim user without account allowed'; EXCEPTION WHEN foreign_key_violation THEN NULL; END;
 END;
END $$;
ROLLBACK;
