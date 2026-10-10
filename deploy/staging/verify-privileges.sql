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
 -- Budget totals (0015): trigger-maintained counters; no removal, no re-keying, triggers present.
 IF has_table_privilege('gateway_runtime','public.budget_totals','DELETE,TRUNCATE') THEN RAISE EXCEPTION 'budget totals removable'; END IF;
 FOREACH t IN ARRAY ARRAY['scope_kind','scope_id','period','period_start'] LOOP
  IF has_column_privilege('gateway_runtime','public.budget_totals',t,'UPDATE') THEN RAISE EXCEPTION 'budget totals re-keyable: %',t; END IF;
 END LOOP;
 IF NOT has_table_privilege('gateway_runtime','public.budget_totals','SELECT,INSERT') OR NOT has_column_privilege('gateway_runtime','public.budget_totals','held_microusd','UPDATE') THEN RAISE EXCEPTION 'budget totals not maintainable by runtime'; END IF;
 IF (SELECT count(*) FROM pg_trigger WHERE tgname LIKE 'budget_totals_%' AND tgenabled='O' AND NOT tgisinternal)<>6 THEN RAISE EXCEPTION 'budget totals triggers missing or disabled'; END IF;
 IF NOT has_column_privilege('gateway_runtime','public.budget_totals','held_unknown_microusd','UPDATE') THEN RAISE EXCEPTION 'budget totals detail not maintainable by runtime'; END IF;
 -- Rate counters (0024): trigger-maintained; no truncation, no re-keying, in-flight rows never removed.
 FOREACH t IN ARRAY ARRAY['rate_minute_counters','inflight_counters'] LOOP
  IF has_table_privilege('gateway_runtime','public.'||t,'TRUNCATE') THEN RAISE EXCEPTION 'rate counters truncatable: %',t; END IF;
  IF has_column_privilege('gateway_runtime','public.'||t,'scope_kind','UPDATE') OR has_column_privilege('gateway_runtime','public.'||t,'scope_id','UPDATE') THEN RAISE EXCEPTION 'rate counters re-keyable: %',t; END IF;
  IF NOT has_table_privilege('gateway_runtime','public.'||t,'SELECT,INSERT') OR NOT has_column_privilege('gateway_runtime','public.'||t,'requests','UPDATE') THEN RAISE EXCEPTION 'rate counters not maintainable by runtime: %',t; END IF;
 END LOOP;
 IF has_column_privilege('gateway_runtime','public.rate_minute_counters','minute_start','UPDATE') THEN RAISE EXCEPTION 'rate counters re-keyable: minute'; END IF;
 IF has_table_privilege('gateway_runtime','public.inflight_counters','DELETE') THEN RAISE EXCEPTION 'in-flight counters removable'; END IF;
 IF (SELECT count(*) FROM pg_trigger WHERE tgname LIKE 'rate_counters_%' AND tgenabled='O' AND NOT tgisinternal)<>8
  OR NOT EXISTS(SELECT FROM pg_trigger WHERE tgname='rate_minute_counters_retained' AND tgenabled='O') THEN RAISE EXCEPTION 'rate counter triggers missing or disabled'; END IF;
 -- Scoped admission (0027): authority and catalog triggers present and enabled.
 IF (SELECT count(*) FROM pg_trigger WHERE tgname LIKE 'omg_authority_%' AND tgenabled='O' AND NOT tgisinternal)<>17
  OR (SELECT count(*) FROM pg_trigger WHERE tgname LIKE 'omg_catalog_%' AND tgenabled='O' AND NOT tgisinternal)<>6 THEN RAISE EXCEPTION 'scope lock triggers missing or disabled'; END IF;
 -- Realtime (0017): response rows are append-then-settle-once; identity and holds fixed.
 IF has_table_privilege('gateway_runtime','public.realtime_responses','DELETE,TRUNCATE') THEN RAISE EXCEPTION 'realtime responses removable'; END IF;
 FOREACH t IN ARRAY ARRAY['execution_id','sequence','window_hold_microusd','created_at'] LOOP
  IF has_column_privilege('gateway_runtime','public.realtime_responses',t,'UPDATE') THEN RAISE EXCEPTION 'mutable realtime response identity: %',t; END IF;
 END LOOP;
 IF (SELECT count(*) FROM pg_trigger WHERE tgname LIKE 'realtime_responses_%' AND tgenabled='O' AND NOT tgisinternal)<>3 THEN RAISE EXCEPTION 'realtime response guards missing or disabled'; END IF;
 -- Async jobs (0016): no removal; identity, ownership and reservation link fixed.
 -- upstream_id is written once (0021: native batches are submitted after creation; trigger).
 FOREACH t IN ARRAY ARRAY['async_jobs','async_job_files'] LOOP
  IF has_table_privilege('gateway_runtime','public.'||t,'DELETE,TRUNCATE') THEN RAISE EXCEPTION 'async job history removable: %',t; END IF;
 END LOOP;
 FOREACH t IN ARRAY ARRAY['id','kind','workspace_id','api_key_id','deployment_id','execution_id','public_model','provider','created_at','poll_deadline_at','video_seconds','video_size','batch_endpoint'] LOOP
  IF has_column_privilege('gateway_runtime','public.async_jobs',t,'UPDATE') THEN RAISE EXCEPTION 'mutable async job identity: %',t; END IF;
 END LOOP;
 FOREACH t IN ARRAY ARRAY['id','workspace_id','api_key_id','deployment_id','public_model','upstream_id','purpose','bytes','request_count','output_token_sum','max_line_output','created_at'] LOOP
  IF has_column_privilege('gateway_runtime','public.async_job_files',t,'UPDATE') THEN RAISE EXCEPTION 'mutable async job file: %',t; END IF;
 END LOOP;
 IF has_column_privilege('gateway_runtime','public.governance_reservations','request_count','UPDATE') OR has_column_privilege('gateway_runtime','public.governance_reservations','admitted_at','UPDATE') THEN RAISE EXCEPTION 'mutable reservation admission snapshot'; END IF;
 IF (SELECT count(*) FROM pg_trigger WHERE tgname LIKE 'async_job%' AND tgenabled='O' AND NOT tgisinternal)<>4 THEN RAISE EXCEPTION 'async job guards missing or disabled'; END IF;
 IF has_table_privilege('gateway_runtime','public.installation_settings','INSERT,DELETE,TRUNCATE') OR has_column_privilege('gateway_runtime','public.installation_settings','singleton','UPDATE') THEN RAISE EXCEPTION 'installation settings row replaceable'; END IF;
 -- Installation logo (0023): a reviewed column set, guarded by a trigger.
 IF NOT has_column_privilege('gateway_runtime','public.installation_settings','branding_logo_file_id','UPDATE') THEN RAISE EXCEPTION 'installation logo not maintainable by runtime'; END IF;
 IF (SELECT count(*) FROM pg_trigger WHERE tgname='installation_settings_logo_guard' AND tgenabled='O' AND NOT tgisinternal)<>1 THEN RAISE EXCEPTION 'installation logo guard missing or disabled'; END IF;
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
 -- File store (0019): metadata rows are never removed; identity/ownership fixed; guard present.
 IF has_table_privilege('gateway_runtime','public.stored_files','DELETE,TRUNCATE') THEN RAISE EXCEPTION 'stored file metadata removable'; END IF;
 FOREACH t IN ARRAY ARRAY['id','object_key','purpose','workspace_id','created_by_user_id','created_by_api_key_id','backend','encryption_key_id','created_at'] LOOP
  IF has_column_privilege('gateway_runtime','public.stored_files',t,'UPDATE') THEN RAISE EXCEPTION 'mutable stored file identity: %',t; END IF;
 END LOOP;
 IF NOT has_table_privilege('gateway_runtime','public.stored_files','SELECT,INSERT') OR NOT has_column_privilege('gateway_runtime','public.stored_files','deleted_at','UPDATE') THEN RAISE EXCEPTION 'stored files not maintainable by runtime'; END IF;
 IF (SELECT count(*) FROM pg_trigger WHERE tgname='stored_files_guard' AND tgenabled='O' AND NOT tgisinternal)<>1 THEN RAISE EXCEPTION 'stored file guard missing or disabled'; END IF;
 -- Files API (0020): api_purpose written once (trigger); reservations runtime-maintained; usage history append-only; progress forward-only.
 IF NOT has_column_privilege('gateway_runtime','public.stored_files','reserved_bytes','UPDATE') OR NOT has_column_privilege('gateway_runtime','public.stored_files','api_purpose','UPDATE') THEN RAISE EXCEPTION 'upload reservations/purpose not maintainable'; END IF;
 IF has_table_privilege('gateway_runtime','public.storage_usage_hours','UPDATE') OR has_table_privilege('gateway_runtime','public.storage_usage_hours','DELETE,TRUNCATE') THEN RAISE EXCEPTION 'storage usage history mutable'; END IF;
 IF has_any_column_privilege('gateway_runtime','public.storage_usage_hours','UPDATE') THEN RAISE EXCEPTION 'storage usage history columns mutable'; END IF;
 IF has_table_privilege('gateway_runtime','public.storage_usage_progress','INSERT') OR has_table_privilege('gateway_runtime','public.storage_usage_progress','DELETE,TRUNCATE') THEN RAISE EXCEPTION 'storage usage progress re-keyable'; END IF;
 IF NOT has_table_privilege('gateway_runtime','public.storage_usage_hours','SELECT,INSERT') THEN RAISE EXCEPTION 'storage usage not recordable'; END IF;
 IF (SELECT count(*) FROM pg_trigger WHERE tgname IN ('storage_usage_hours_guard','storage_usage_progress_guard') AND tgenabled='O' AND NOT tgisinternal)<>2 THEN RAISE EXCEPTION 'storage usage guards missing or disabled'; END IF;
 -- Batch engine (0021): line/segment history never removed; identity fixed; pinned tier and line link fixed; guards present.
 FOREACH t IN ARRAY ARRAY['batch_lines','batch_segments'] LOOP
  IF has_table_privilege('gateway_runtime','public.'||t,'DELETE,TRUNCATE') THEN RAISE EXCEPTION 'batch history removable: %',t; END IF;
 END LOOP;
 FOREACH t IN ARRAY ARRAY['job_id','workspace_id','line_no','started_at'] LOOP
  IF has_column_privilege('gateway_runtime','public.batch_lines',t,'UPDATE') THEN RAISE EXCEPTION 'mutable batch line identity: %',t; END IF;
 END LOOP;
 IF has_any_column_privilege('gateway_runtime','public.batch_segments','UPDATE') THEN RAISE EXCEPTION 'batch segments mutable'; END IF;
 FOREACH t IN ARRAY ARRAY['batch_mode','user_id','input_file_id','work_file_id','price_tier','retry_limit'] LOOP
  IF has_column_privilege('gateway_runtime','public.async_jobs',t,'UPDATE') THEN RAISE EXCEPTION 'mutable batch identity: %',t; END IF;
 END LOOP;
 IF has_column_privilege('gateway_runtime','public.governance_reservations','price_tier','UPDATE') OR has_column_privilege('gateway_runtime','public.inference_executions','batch_job_id','UPDATE') OR has_column_privilege('gateway_runtime','public.deployment_prices','batch_price_lines','UPDATE') THEN RAISE EXCEPTION 'mutable batch pricing snapshot'; END IF;
 IF (SELECT count(*) FROM pg_trigger WHERE tgname IN ('batch_lines_guard','batch_lines_no_delete','batch_segments_no_update') AND tgenabled='O' AND NOT tgisinternal)<>3 THEN RAISE EXCEPTION 'batch guards missing or disabled'; END IF;
 -- Batch scheduling (0022): a line's route and a batch's window are fixed; settings and signals are never removed or re-keyed; demand rows keep their identity.
 IF has_column_privilege('gateway_runtime','public.batch_lines','deployment_id','UPDATE') OR has_column_privilege('gateway_runtime','public.async_jobs','completion_window_hours','UPDATE') THEN RAISE EXCEPTION 'mutable batch scheduling snapshot'; END IF;
 FOREACH t IN ARRAY ARRAY['deployment_batch_scheduling','deployment_batch_signals'] LOOP
  IF has_table_privilege('gateway_runtime','public.'||t,'DELETE,TRUNCATE') OR has_column_privilege('gateway_runtime','public.'||t,'deployment_id','UPDATE') THEN RAISE EXCEPTION 'batch route state removable or re-keyable: %',t; END IF;
 END LOOP;
 FOREACH t IN ARRAY ARRAY['job_id','workspace_id','deployment_id','since'] LOOP
  IF has_column_privilege('gateway_runtime','public.batch_route_waits',t,'UPDATE') THEN RAISE EXCEPTION 'mutable batch demand identity: %',t; END IF;
 END LOOP;
 IF has_table_privilege('gateway_runtime','public.batch_route_waits','TRUNCATE') THEN RAISE EXCEPTION 'batch demand truncatable'; END IF;
 IF EXISTS(SELECT FROM pg_proc p JOIN pg_namespace n ON n.oid=p.pronamespace WHERE n.nspname='public' AND has_function_privilege('gateway_runtime',p.oid,'EXECUTE') AND p.proname NOT IN ('lock_installation','workspace_model_allowed','valid_model_protocols','valid_i64_string','valid_cache_pricing','valid_billing_usage','valid_cost_components','components_total','valid_meter_usage','valid_meter_variant','valid_price_lines','valid_max_units','valid_model_protocols_base','valid_cost_components_base','valid_price_lines_base','valid_upstream_job_id','rate_reserved_tokens','rate_contribution','omg_scope_key','omg_type_key','omg_scope_lock_audit_order','omg_lock_scopes','omg_admission_locks','omg_lock_scope_rows','omg_catalog_lock_mode','omg_scope_lock_exclusive')) THEN RAISE EXCEPTION 'unexpected executable function'; END IF;
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
 -- No installation layer (0026): neither its table nor installation budget rows exist.
 IF to_regclass('public.installation_policy') IS NOT NULL THEN RAISE EXCEPTION 'installation policy layer still present'; END IF;
 BEGIN INSERT INTO policy_budgets(layer,period,amount_microusd) VALUES('installation','day',1); RAISE EXCEPTION 'installation budgets representable'; EXCEPTION WHEN check_violation THEN NULL; END;
 INSERT INTO workspace_type_policies(kind) VALUES('project') ON CONFLICT(kind) DO UPDATE SET tokens_per_minute=EXCLUDED.tokens_per_minute;
 INSERT INTO workspace_platform_policy_overrides(workspace_id) VALUES(ws) ON CONFLICT(workspace_id) DO UPDATE SET concurrent_requests=EXCLUDED.concurrent_requests;
 INSERT INTO workspace_local_policies(workspace_id) VALUES(ws) ON CONFLICT(workspace_id) DO NOTHING;
 INSERT INTO key_policies(workspace_id,governance_key_id) VALUES(ws,k) ON CONFLICT(workspace_id,governance_key_id) DO NOTHING;
 -- Jobs at once (0018): every policy layer stores and upserts concurrent_jobs; type defaults start at 2.
 INSERT INTO workspace_type_policies(kind,concurrent_jobs) VALUES('team',3) ON CONFLICT(kind) DO UPDATE SET concurrent_jobs=EXCLUDED.concurrent_jobs;
 INSERT INTO workspace_platform_policy_overrides(workspace_id,concurrent_jobs) VALUES(ws,4) ON CONFLICT(workspace_id) DO UPDATE SET concurrent_jobs=EXCLUDED.concurrent_jobs;
 INSERT INTO workspace_local_policies(workspace_id,concurrent_jobs) VALUES(ws,2) ON CONFLICT(workspace_id) DO UPDATE SET concurrent_jobs=EXCLUDED.concurrent_jobs;
 INSERT INTO key_policies(workspace_id,governance_key_id,concurrent_jobs) VALUES(ws,k,1) ON CONFLICT(workspace_id,governance_key_id) DO UPDATE SET concurrent_jobs=EXCLUDED.concurrent_jobs;
 IF (SELECT concurrent_jobs FROM workspace_type_policies WHERE kind='personal') IS DISTINCT FROM 2 THEN RAISE EXCEPTION 'jobs at once type default missing'; END IF;
 BEGIN INSERT INTO key_policies(workspace_id,governance_key_id,concurrent_jobs) VALUES(ws,k,0) ON CONFLICT(workspace_id,governance_key_id) DO UPDATE SET concurrent_jobs=EXCLUDED.concurrent_jobs; RAISE EXCEPTION 'non-positive jobs at once allowed'; EXCEPTION WHEN check_violation THEN NULL; END;
 PERFORM count(*) FROM governance_reservations r JOIN inference_executions e ON e.id=r.execution_id LEFT JOIN async_jobs j ON j.execution_id=r.execution_id WHERE r.state='pending' AND e.workload_kind IN('videos','batches') AND j.state NOT IN('completed','failed','cancelled','expired') AND j.cancel_requested_at IS NULL;
 INSERT INTO policy_budgets(layer,kind,period,amount_microusd) VALUES('type','project','week',1),('type','project','lifetime',9);
 INSERT INTO policy_budgets(layer,workspace_id,period,amount_microusd) VALUES('override',ws,'month',1),('local',ws,'day',1),('local',ws,'month',2);
 INSERT INTO policy_budgets(layer,workspace_id,governance_key_id,period,amount_microusd) VALUES('key',ws,k,'lifetime',1);
 DELETE FROM policy_budgets WHERE layer='local' AND workspace_id=ws AND period='day';
 IF (SELECT count(*) FROM policy_budgets WHERE workspace_id=ws)<>3 THEN RAISE EXCEPTION 'stacked budget replacement failed'; END IF;
 BEGIN INSERT INTO policy_budgets(layer,workspace_id,period,amount_microusd) VALUES('local',ws,'month',3); RAISE EXCEPTION 'one budget per scope and period absent'; EXCEPTION WHEN unique_violation THEN NULL; END;
 BEGIN INSERT INTO policy_budgets(layer,kind,period,amount_microusd) VALUES('type','team','year',1); RAISE EXCEPTION 'budget period constraint absent'; EXCEPTION WHEN check_violation THEN NULL; END;
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
 -- Budget totals (0015): the runtime's reservation writes maintained them; unknown keeps its hold.
 IF (SELECT (held_microusd,settled_microusd,reservations,pending,unknown,held_unknown_microusd)::text FROM budget_totals WHERE scope_kind='key' AND scope_id=k AND period='lifetime')<>'(1,0,1,0,1,1)' THEN RAISE EXCEPTION 'budget totals not maintained'; END IF;
 -- No installation scope (0026): no request writes a global totals row; installation spend sums workspace rows.
 IF EXISTS(SELECT FROM budget_totals WHERE scope_kind NOT IN ('workspace','key')) THEN RAISE EXCEPTION 'installation totals scope maintained'; END IF;
 PERFORM coalesce(sum(t.settled_microusd+t.held_microusd-t.held_unknown_microusd),0),coalesce(sum(t.unknown+t.unresolved-t.unresolved_unknown),0) FROM budget_totals t WHERE t.scope_kind='workspace' AND t.period='month' AND t.period_start=date_trunc('month',now(),'UTC');
 -- Scoped admission (0027): the runtime takes admission's shared scope locks,
 -- locks (creating) totals/counter rows, and management's exclusive scope locks.
 PERFORM * FROM omg_admission_locks(ws,NULL,k);
 PERFORM omg_lock_scope_rows(ARRAY[ws],ARRAY[k],ARRAY[now()],true);
 PERFORM omg_lock_scopes(ARRAY[72419511,72419513],ARRAY[omg_scope_key(ws),omg_scope_key(k)],true);
 IF (SELECT count(*) FROM pg_locks WHERE locktype='advisory' AND pid=pg_backend_pid() AND objsubid=2 AND classid::bigint BETWEEN 72419510 AND 72419513)<3 THEN RAISE EXCEPTION 'scope locks not taken'; END IF;
 -- Rate counters (0024): the same writes maintained them; unknown released the in-flight slot.
 IF (SELECT (requests,unreserved,tokens)::text FROM rate_minute_counters WHERE minute_start=date_trunc('minute',now(),'UTC') AND scope_kind='key' AND scope_id=k)<>'(1,0,110)'
  OR (SELECT requests FROM inflight_counters WHERE scope_kind='key' AND scope_id=k)<>0 THEN RAISE EXCEPTION 'rate counters not maintained'; END IF;
 PERFORM rate_contribution(r,'generation',NULL,false,NULL,NULL,1) FROM governance_reservations r WHERE execution_id=e;
 BEGIN DELETE FROM rate_minute_counters WHERE scope_id=k; RAISE EXCEPTION 'retained rate counters removable'; EXCEPTION WHEN raise_exception THEN IF SQLERRM NOT LIKE 'rate counters of the retained window%' THEN RAISE; END IF; END;
 BEGIN DELETE FROM inflight_counters WHERE scope_id=k; RAISE EXCEPTION 'in-flight counters removal allowed'; EXCEPTION WHEN insufficient_privilege THEN NULL; END;
 BEGIN UPDATE rate_minute_counters SET minute_start='epoch' WHERE scope_id=k; RAISE EXCEPTION 'rate counters re-key allowed'; EXCEPTION WHEN insufficient_privilege THEN NULL; END;
 BEGIN TRUNCATE TABLE inflight_counters; RAISE EXCEPTION 'in-flight counters truncate allowed'; EXCEPTION WHEN insufficient_privilege THEN NULL; END;
 PERFORM coalesce(sum(t.settled_microusd+t.held_microusd),0),bool_or(t.unresolved+t.unreserved_executions>0) FROM unnest(ARRAY['workspace'],ARRAY[ws],ARRAY['month'],ARRAY[date_trunc('month',now(),'UTC')]) q(kind,id,period,start) LEFT JOIN budget_totals t ON t.scope_kind=q.kind AND t.scope_id=q.id AND t.period=q.period AND t.period_start=q.start;
 BEGIN DELETE FROM budget_totals WHERE scope_id=k; RAISE EXCEPTION 'budget totals removal allowed'; EXCEPTION WHEN insufficient_privilege THEN NULL; END;
 BEGIN UPDATE budget_totals SET period_start='epoch' WHERE scope_id=k; RAISE EXCEPTION 'budget totals re-key allowed'; EXCEPTION WHEN insufficient_privilege THEN NULL; END;
 BEGIN TRUNCATE TABLE budget_totals; RAISE EXCEPTION 'budget totals truncate allowed'; EXCEPTION WHEN insufficient_privilege THEN NULL; END;
 -- Async jobs (0016): a job is recorded, its lease extended, polled forward,
 -- settled once; batch files are claimed, consumed and linked to outputs.
 DECLARE j uuid:=gen_random_uuid(); fi uuid:=gen_random_uuid(); fo uuid:=gen_random_uuid(); BEGIN
  UPDATE governance_reservations SET lease_expires_at=greatest(lease_expires_at,now()+interval '6 hours') WHERE execution_id=e AND state='pending';
  INSERT INTO async_job_files(id,workspace_id,api_key_id,deployment_id,public_model,upstream_id,purpose,bytes,endpoint,request_count,output_token_sum,max_line_output) VALUES(fi,ws,k,d,'rollback','file-in-'||fi,'batch',10,'/v1/chat/completions',2,20,10);
  UPDATE async_job_files SET claimed_by_execution_id=e WHERE id=fi AND claimed_by_execution_id IS NULL;
  INSERT INTO async_jobs(id,kind,workspace_id,api_key_id,deployment_id,execution_id,public_model,provider,upstream_id,poll_deadline_at,batch_endpoint) VALUES(j,'batch',ws,k,d,e,'rollback','openai_compatible','batch_up_'||replace(j::text,'-',''),now()+interval '26 hours','/v1/chat/completions');
  UPDATE async_job_files SET job_id=j WHERE id=fi AND job_id IS NULL;
  INSERT INTO async_job_files(id,workspace_id,api_key_id,deployment_id,public_model,upstream_id,purpose,job_id) VALUES(fo,ws,k,d,'rollback','file-out-'||fo,'batch_output',j) ON CONFLICT DO NOTHING;
  PERFORM id FROM async_jobs WHERE id IN(SELECT id FROM async_jobs WHERE settled_at IS NULL AND next_poll_at<=clock_timestamp() ORDER BY next_poll_at LIMIT 1 FOR UPDATE SKIP LOCKED);
  UPDATE async_jobs SET next_poll_at=clock_timestamp()+make_interval(secs=>30) WHERE id=j;
  UPDATE async_jobs SET state='in_progress',upstream_status='finalizing',progress=50,request_total=2,request_completed=1,request_failed=0,last_polled_at=clock_timestamp(),poll_failures=0 WHERE id=j AND state='queued';
  UPDATE async_jobs SET poll_failures=poll_failures+1 WHERE id=j;
  UPDATE async_jobs SET cancel_requested_at=now() WHERE id=j;
  UPDATE async_jobs SET state='completed',completed_at=now(),expires_at=now()+interval '1 day',error_code=NULL WHERE id=j;
  UPDATE async_jobs SET settled_at=clock_timestamp() WHERE id=j AND settled_at IS NULL;
  PERFORM coalesce((SELECT state='pending' FROM governance_reservations WHERE execution_id=e),false);
  BEGIN UPDATE async_jobs SET state='in_progress' WHERE id=j; RAISE EXCEPTION 'async job regression allowed'; EXCEPTION WHEN raise_exception THEN IF SQLERRM NOT LIKE 'invalid async job transition%' THEN RAISE; END IF; END;
  BEGIN UPDATE async_jobs SET workspace_id=personal WHERE id=j; RAISE EXCEPTION 'async job re-own allowed'; EXCEPTION WHEN insufficient_privilege THEN NULL; END;
  BEGIN DELETE FROM async_jobs WHERE id=j; RAISE EXCEPTION 'async job removal allowed'; EXCEPTION WHEN insufficient_privilege THEN NULL; END;
  BEGIN UPDATE async_job_files SET upstream_id='x' WHERE id=fi; RAISE EXCEPTION 'async job file rewrite allowed'; EXCEPTION WHEN insufficient_privilege THEN NULL; END;
  BEGIN UPDATE async_job_files SET job_id=NULL WHERE id=fi; RAISE EXCEPTION 'consumed batch file released'; EXCEPTION WHEN raise_exception THEN IF SQLERRM<>'async job files are immutable' THEN RAISE; END IF; END;
  BEGIN INSERT INTO async_jobs(id,kind,workspace_id,api_key_id,deployment_id,execution_id,public_model,provider,upstream_id,poll_deadline_at,video_seconds,video_size) VALUES(gen_random_uuid(),'video',personal,k,d,e,'rollback','openai','video_x',now(),4,'720x1280'); RAISE EXCEPTION 'cross-workspace job allowed'; EXCEPTION WHEN foreign_key_violation OR unique_violation THEN NULL; END;
  IF NOT valid_model_protocols(ARRAY['videos']) OR NOT valid_model_protocols(ARRAY['batches']) OR valid_model_protocols(ARRAY['videos','batches'])
   OR NOT valid_price_lines('[{"meter":"output_video_seconds_ms","microusd_per_batch":"100000","batch":1000,"unit_label":"/second","sku_label":"Video","variant":"720x1280"}]'::jsonb)
   OR NOT valid_meter_usage('{"output_images":"0","input_characters":"0","input_audio_seconds_ms":"0","output_audio_seconds_ms":"0","search_units":"0","requests":"1","output_video_seconds_ms":"8000"}'::jsonb) THEN RAISE EXCEPTION 'async job validators'; END IF;
 END;
 -- Realtime (0017): a session extends its hold, records a response, settles it once.
 UPDATE governance_reservations SET reserved_tokens=reserved_tokens+1,held_microusd=held_microusd+1 WHERE execution_id=e;
 INSERT INTO realtime_responses(execution_id,sequence,window_hold_microusd) VALUES(e,1,1);
 UPDATE realtime_responses SET state='settled',status='completed',actual_microusd=0,input_text_tokens=0,cached_text_tokens=0,input_audio_tokens=0,cached_audio_tokens=0,output_text_tokens=0,output_audio_tokens=0,completed_at=clock_timestamp() WHERE execution_id=e AND sequence=1;
 BEGIN UPDATE realtime_responses SET status='failed' WHERE execution_id=e; RAISE EXCEPTION 'settled realtime response rewritten'; EXCEPTION WHEN raise_exception THEN IF SQLERRM<>'realtime responses settle once' THEN RAISE; END IF; END;
 BEGIN DELETE FROM realtime_responses WHERE execution_id=e; RAISE EXCEPTION 'realtime response removal allowed'; EXCEPTION WHEN insufficient_privilege THEN NULL; END;
 IF NOT valid_model_protocols(ARRAY['realtime']) OR valid_model_protocols(ARRAY['realtime','chat_completions'])
  OR NOT valid_price_lines('[{"meter":"input_tokens","not_applicable":true},{"meter":"output_audio_tokens","microusd_per_batch":"64000000","batch":1000000,"unit_label":"/M tokens","sku_label":"Audio output"}]'::jsonb)
  OR valid_price_lines('[{"meter":"output_audio_tokens","not_applicable":true}]'::jsonb) THEN RAISE EXCEPTION 'realtime validators'; END IF;
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
  -- Installation spend rules (0026): created and edited as runtime; never workspace-scoped.
  DECLARE sr uuid:=gen_random_uuid(); BEGIN
   INSERT INTO alert_rules(id,scope,kind,name,thresholds,spend_period,spend_amount_microusd,notify_platform_admins,created_by,updated_by) VALUES(sr,'installation','spend_threshold','Rollback spend',ARRAY[80,100],'month',1000000,true,u,u);
   UPDATE alert_rules SET spend_period='week',spend_amount_microusd=2000000,thresholds=ARRAY[50],updated_at=now(),updated_by=u WHERE id=sr;
   BEGIN INSERT INTO alert_rules(id,scope,workspace_id,kind,name,thresholds,spend_period,spend_amount_microusd) VALUES(gen_random_uuid(),'workspace',ws,'spend_threshold','x',ARRAY[50],'day',1); RAISE EXCEPTION 'workspace spend rule allowed'; EXCEPTION WHEN check_violation THEN NULL; END;
   BEGIN INSERT INTO alert_rules(id,scope,kind,name,budget_layers,thresholds) VALUES(gen_random_uuid(),'installation','budget_threshold','x',ARRAY['installation'],ARRAY[50]); RAISE EXCEPTION 'installation budget layer alert allowed'; EXCEPTION WHEN check_violation THEN NULL; END;
  END;
  UPDATE alert_rules SET name='Rollback budgets 2',enabled=false,thresholds=ARRAY[90],notify_emails='{}',updated_at=now(),updated_by=u WHERE id=ar;
  PERFORM id FROM alert_rules WHERE id=ar FOR UPDATE;
  INSERT INTO alert_events(id,rule_id,kind,subject_key,level,severity,workspace_id,summary,details) VALUES(ev,ar,'budget_threshold','local:'||ws||':-:month',80,'warning',ws,'Workspace monthly budget reached 80%','{"used_microusd":"8"}');
  INSERT INTO alert_events(id,rule_id,kind,subject_key,level,severity,workspace_id,summary) VALUES(gen_random_uuid(),ar,'budget_threshold','local:'||ws||':-:month',100,'critical',ws,'Duplicate') ON CONFLICT DO NOTHING;
  IF (SELECT count(*) FROM alert_events WHERE rule_id=ar AND resolved_at IS NULL)<>1 THEN RAISE EXCEPTION 'duplicate open alert allowed'; END IF;
  INSERT INTO alert_events(id,builtin,kind,subject_key,level,severity,workspace_id,summary) VALUES(gen_random_uuid(),'personal_budget','budget_threshold','key:'||personal||':-:day',100,'critical',personal,'API key daily budget reached 100%');
  -- SCIM last-admin incident (0018): installation-wide built-in, no workspace.
  INSERT INTO alert_events(id,builtin,kind,subject_key,level,severity,summary) VALUES(gen_random_uuid(),'scim_last_admin','scim_last_admin','scim_last_admin',1,'critical','SCIM tried to remove the last Platform Admin');
  BEGIN INSERT INTO alert_events(id,builtin,kind,subject_key,level,severity,workspace_id,summary) VALUES(gen_random_uuid(),'scim_last_admin','scim_last_admin','x',1,'critical',ws,'x'); RAISE EXCEPTION 'scoped scim alert allowed'; EXCEPTION WHEN check_violation THEN NULL; END;
  BEGIN INSERT INTO alert_events(id,rule_id,kind,subject_key,level,severity,summary) VALUES(gen_random_uuid(),ar,'scim_last_admin','y',1,'critical','x'); RAISE EXCEPTION 'rule scim alert allowed'; EXCEPTION WHEN check_violation THEN NULL; END;
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
 -- File store (0019): pending → committed once → swept (deleted_at, names cleared); settings.
 DECLARE fid uuid:=gen_random_uuid(); lid uuid:=gen_random_uuid(); x1 uuid:=gen_random_uuid(); x2 uuid:=gen_random_uuid(); x3 uuid:=gen_random_uuid(); BEGIN
  UPDATE installation_settings SET file_batch_enabled=true,file_batch_retention_days=3,file_video_enabled=true,file_video_retention_days=7,file_user_files_enabled=false,file_user_files_retention_days=30,file_export_retention_days=1,file_store_last_check_at=now(),file_store_last_check_ok=false,file_store_last_check_error='timeout',file_store_last_check_target=repeat('a',64) WHERE singleton;
  BEGIN UPDATE installation_settings SET file_batch_retention_days=0 WHERE singleton; RAISE EXCEPTION 'file retention bound absent'; EXCEPTION WHEN check_violation THEN NULL; END;
  INSERT INTO stored_files(id,object_key,purpose,workspace_id,created_by_user_id,created_by_api_key_id,filename,content_type,backend,encryption_key_id,expires_at) VALUES(fid,'batch_input/'||ws||'/'||fid,'batch_input',ws,u,k,'input.jsonl','application/jsonl','s3','k2026',now()+interval '1 day');
  INSERT INTO stored_files(id,object_key,purpose,backend,encryption_key_id) VALUES(lid,'branding/installation/'||lid,'branding','local','k2026');
  UPDATE stored_files SET size_bytes=10,sha256=decode(repeat('ab',32),'hex'),committed_at=clock_timestamp() WHERE id=fid AND committed_at IS NULL AND deleted_at IS NULL;
  PERFORM coalesce(sum(size_bytes),0) FROM stored_files WHERE workspace_id=ws AND deleted_at IS NULL;
  PERFORM purpose,count(*) FROM stored_files f CROSS JOIN installation_settings s WHERE s.singleton AND f.deleted_at IS NULL AND f.created_at+make_interval(days=>s.file_batch_retention_days)<=now() GROUP BY purpose;
  UPDATE stored_files f SET last_delete_attempt_at=clock_timestamp() FROM (SELECT id FROM stored_files WHERE deleted_at IS NULL ORDER BY created_at LIMIT 10 FOR UPDATE SKIP LOCKED) due WHERE f.id=due.id;
  UPDATE stored_files SET delete_attempts=delete_attempts+1,last_delete_error='unavailable',expires_at=least(expires_at,clock_timestamp()) WHERE id=fid;
  UPDATE stored_files SET deleted_at=clock_timestamp(),filename=NULL,content_type=NULL WHERE id=fid AND deleted_at IS NULL;
  BEGIN UPDATE stored_files SET deleted_at=NULL WHERE id=fid; RAISE EXCEPTION 'stored file undeleted'; EXCEPTION WHEN raise_exception THEN IF SQLERRM<>'deleted stored files are final' THEN RAISE; END IF; END;
  BEGIN UPDATE stored_files SET size_bytes=1,sha256=decode(repeat('cd',32),'hex'),committed_at=now() WHERE id=lid; UPDATE stored_files SET size_bytes=2 WHERE id=lid; RAISE EXCEPTION 'stored file contents rewritten'; EXCEPTION WHEN raise_exception THEN IF SQLERRM<>'stored file contents are written once' THEN RAISE; END IF; END;
  BEGIN DELETE FROM stored_files WHERE id=lid; RAISE EXCEPTION 'stored file row removal allowed'; EXCEPTION WHEN insufficient_privilege THEN NULL; END;
  BEGIN UPDATE stored_files SET workspace_id=personal WHERE id=lid; RAISE EXCEPTION 'stored file re-own allowed'; EXCEPTION WHEN insufficient_privilege THEN NULL; END;
  BEGIN UPDATE stored_files SET encryption_key_id='other' WHERE id=lid; RAISE EXCEPTION 'stored file key id rewrite allowed'; EXCEPTION WHEN insufficient_privilege THEN NULL; END;
  BEGIN INSERT INTO stored_files(id,object_key,purpose,workspace_id,backend,encryption_key_id) VALUES(gen_random_uuid(),'batch_input/'||ws||'/../x','batch_input',ws,'s3','k'); RAISE EXCEPTION 'non-canonical object key allowed'; EXCEPTION WHEN check_violation THEN NULL; END;
  BEGIN INSERT INTO stored_files(id,object_key,purpose,backend,encryption_key_id) VALUES(x1,'batch_output/installation/'||x1,'batch_output','s3','k'); RAISE EXCEPTION 'installation-scoped customer content allowed'; EXCEPTION WHEN check_violation THEN NULL; END;
  BEGIN INSERT INTO stored_files(id,object_key,purpose,workspace_id,filename,backend,encryption_key_id) VALUES(x2,'user_file/'||ws||'/'||x2,'user_file',ws,'../etc/passwd','s3','k'); RAISE EXCEPTION 'path-like filename allowed'; EXCEPTION WHEN check_violation THEN NULL; END;
  BEGIN INSERT INTO stored_files(id,object_key,purpose,workspace_id,created_by_api_key_id,backend,encryption_key_id) VALUES(x3,'export/'||personal||'/'||x3,'export',personal,k,'s3','k'); RAISE EXCEPTION 'cross-workspace key attribution allowed'; EXCEPTION WHEN foreign_key_violation THEN NULL; END;
  -- Installation logo (0023): upload (committed branding file referenced), replace, remove.
  DECLARE l1 uuid:=gen_random_uuid(); l2 uuid:=gen_random_uuid(); BEGIN
   INSERT INTO stored_files(id,object_key,purpose,created_by_user_id,filename,content_type,backend,encryption_key_id) VALUES(l1,'branding/installation/'||l1,'branding',u,'logo.png','image/png','local','k2026'),(l2,'branding/installation/'||l2,'branding',u,'logo.webp','image/webp','local','k2026');
   BEGIN UPDATE installation_settings SET branding_logo_file_id=l1,branding_logo_updated_at=now(),branding_logo_width=64,branding_logo_height=64 WHERE singleton; RAISE EXCEPTION 'uncommitted logo referenced'; EXCEPTION WHEN raise_exception THEN IF SQLERRM<>'installation logo must be a live branding file' THEN RAISE; END IF; END;
   UPDATE stored_files SET size_bytes=100,sha256=decode(repeat('ab',32),'hex'),committed_at=clock_timestamp() WHERE id IN (l1,l2) AND committed_at IS NULL AND deleted_at IS NULL;
   PERFORM branding_logo_file_id FROM installation_settings WHERE singleton;
   UPDATE installation_settings SET branding_logo_file_id=l1,branding_logo_updated_at=now(),branding_logo_width=64,branding_logo_height=64,logo_url=NULL,updated_at=now(),updated_by=u WHERE singleton;
   UPDATE installation_settings SET branding_logo_file_id=l2,branding_logo_updated_at=now(),branding_logo_width=128,branding_logo_height=96,updated_at=now(),updated_by=u WHERE singleton;
   UPDATE stored_files SET deleted_at=clock_timestamp(),filename=NULL,content_type=NULL WHERE id=l1 AND deleted_at IS NULL;
   BEGIN UPDATE installation_settings SET branding_logo_file_id=l1 WHERE singleton; RAISE EXCEPTION 'deleted logo referenced'; EXCEPTION WHEN raise_exception THEN IF SQLERRM<>'installation logo must be a live branding file' THEN RAISE; END IF; END;
   BEGIN UPDATE installation_settings SET branding_logo_file_id=fid WHERE singleton; RAISE EXCEPTION 'non-branding logo referenced'; EXCEPTION WHEN raise_exception THEN IF SQLERRM<>'installation logo must be a live branding file' THEN RAISE; END IF; END;
   BEGIN UPDATE installation_settings SET branding_logo_width=NULL WHERE singleton; RAISE EXCEPTION 'partial logo allowed'; EXCEPTION WHEN check_violation THEN NULL; END;
   BEGIN UPDATE installation_settings SET branding_logo_width=8 WHERE singleton; RAISE EXCEPTION 'logo dimension bound absent'; EXCEPTION WHEN check_violation THEN NULL; END;
   UPDATE installation_settings SET branding_logo_file_id=NULL,branding_logo_updated_at=NULL,branding_logo_width=NULL,branding_logo_height=NULL,updated_at=now(),updated_by=u WHERE singleton;
   UPDATE stored_files SET deleted_at=clock_timestamp(),filename=NULL,content_type=NULL WHERE id=l2 AND deleted_at IS NULL;
   INSERT INTO audit_events(id,actor_user_id,action,resource_type,resource_id,metadata) VALUES(gen_random_uuid(),u,'settings.logo_uploaded','installation_settings',l2,'{"kind":"webp","count":100}'),(gen_random_uuid(),u,'settings.logo_removed','installation_settings',l2,'{}');
  END;
 END;
 -- Files API (0020): storage quota layers, a reserved upload, listing, usage rows (append-only).
 DECLARE f2 uuid:=gen_random_uuid(); x4 uuid:=gen_random_uuid(); BEGIN
  UPDATE workspace_type_policies SET storage_bytes=2147483648 WHERE kind='team';
  IF (SELECT storage_bytes FROM workspace_type_policies WHERE kind='personal') IS DISTINCT FROM 1073741824 THEN RAISE EXCEPTION 'storage type default missing'; END IF;
  INSERT INTO workspace_platform_policy_overrides(workspace_id,storage_bytes) VALUES(ws,5000) ON CONFLICT(workspace_id) DO UPDATE SET storage_bytes=EXCLUDED.storage_bytes;
  INSERT INTO workspace_local_policies(workspace_id,storage_bytes) VALUES(ws,4000) ON CONFLICT(workspace_id) DO UPDATE SET storage_bytes=EXCLUDED.storage_bytes;
  BEGIN UPDATE workspace_local_policies SET storage_bytes=0 WHERE workspace_id=ws; RAISE EXCEPTION 'non-positive storage quota allowed'; EXCEPTION WHEN check_violation THEN NULL; END;
  PERFORM pg_advisory_xact_lock_shared(72419502);
  PERFORM pg_advisory_xact_lock(hashtextextended('storage_quota:'||ws::text,0));
  INSERT INTO stored_files(id,object_key,purpose,workspace_id,created_by_user_id,created_by_api_key_id,filename,content_type,backend,encryption_key_id,expires_at,api_purpose) VALUES(f2,'user_file/'||ws||'/'||f2,'user_file',ws,u,k,'photo.png','image/png','s3','k2026',least(NULL::timestamptz,now()+make_interval(secs=>3600)),'vision');
  UPDATE stored_files SET reserved_bytes=greatest(reserved_bytes,100) WHERE id=f2 AND workspace_id=ws AND committed_at IS NULL AND deleted_at IS NULL;
  BEGIN UPDATE stored_files SET reserved_bytes=1 WHERE id=f2; RAISE EXCEPTION 'reservation shrink allowed'; EXCEPTION WHEN raise_exception THEN IF SQLERRM<>'stored file reservations only grow while pending' THEN RAISE; END IF; END;
  UPDATE stored_files SET size_bytes=10,sha256=decode(repeat('ab',32),'hex'),committed_at=clock_timestamp() WHERE id=f2 AND committed_at IS NULL AND deleted_at IS NULL;
  BEGIN UPDATE stored_files SET reserved_bytes=500 WHERE id=f2; RAISE EXCEPTION 'committed reservation changed'; EXCEPTION WHEN raise_exception THEN IF SQLERRM<>'stored file reservations only grow while pending' THEN RAISE; END IF; END;
  BEGIN UPDATE stored_files SET api_purpose='evals' WHERE id=f2; RAISE EXCEPTION 'api purpose rewrite allowed'; EXCEPTION WHEN raise_exception THEN IF SQLERRM<>'stored file api purpose is written once' THEN RAISE; END IF; END;
  BEGIN INSERT INTO stored_files(id,object_key,purpose,workspace_id,backend,encryption_key_id,api_purpose) VALUES(x4,'batch_input/'||ws||'/'||x4,'batch_input',ws,'s3','k','user_data'); RAISE EXCEPTION 'mismatched api purpose allowed'; EXCEPTION WHEN check_violation THEN NULL; END;
  PERFORM coalesce(sum(CASE WHEN committed_at IS NOT NULL THEN size_bytes ELSE reserved_bytes END),0) FROM stored_files WHERE workspace_id=ws AND deleted_at IS NULL;
  PERFORM id FROM stored_files WHERE workspace_id=ws AND api_purpose IS NOT NULL AND deleted_at IS NULL ORDER BY created_at DESC,id DESC LIMIT 101;
  PERFORM recorded_through FROM storage_usage_progress WHERE singleton FOR UPDATE SKIP LOCKED;
  INSERT INTO storage_usage_hours(workspace_id,purpose,hour_start,byte_seconds,file_count) VALUES(ws,'user_file',date_trunc('hour',now(),'UTC')-interval '1 hour',36000,1) ON CONFLICT DO NOTHING;
  UPDATE storage_usage_progress SET recorded_through=recorded_through+interval '1 hour' WHERE singleton;
  PERFORM purpose,sum(byte_seconds) FROM storage_usage_hours WHERE workspace_id=ws GROUP BY purpose;
  BEGIN UPDATE storage_usage_hours SET byte_seconds=1 WHERE workspace_id=ws; RAISE EXCEPTION 'storage usage rewrite allowed'; EXCEPTION WHEN insufficient_privilege THEN NULL; END;
  BEGIN DELETE FROM storage_usage_hours WHERE workspace_id=ws; RAISE EXCEPTION 'storage usage removal allowed'; EXCEPTION WHEN insufficient_privilege THEN NULL; END;
  BEGIN UPDATE storage_usage_progress SET recorded_through=recorded_through-interval '2 hours' WHERE singleton; RAISE EXCEPTION 'storage usage progress moved back'; EXCEPTION WHEN raise_exception THEN IF SQLERRM<>'storage usage progress only moves forward' THEN RAISE; END IF; END;
  BEGIN INSERT INTO storage_usage_hours(workspace_id,purpose,hour_start,byte_seconds,file_count) VALUES(ws,'user_file',date_trunc('hour',now(),'UTC')+interval '90 seconds',1,1); RAISE EXCEPTION 'unaligned usage hour allowed'; EXCEPTION WHEN check_violation THEN NULL; END;
  BEGIN INSERT INTO storage_usage_hours(workspace_id,purpose,hour_start,byte_seconds,file_count) VALUES(ws,'branding',date_trunc('hour',now(),'UTC')-interval '2 hours',1,1); RAISE EXCEPTION 'installation purpose usage allowed'; EXCEPTION WHEN check_violation THEN NULL; END;
 END;
 -- Batch engine (0021): batch prices; a gateway-run batch's envelope, line
 -- claims, transfers, segments, results and close; a native batch's upstream id.
 DECLARE bp uuid:=gen_random_uuid(); env uuid:=gen_random_uuid(); line uuid:=gen_random_uuid(); bj uuid:=gen_random_uuid(); nj uuid:=gen_random_uuid(); nenv uuid:=gen_random_uuid(); fin uuid:=gen_random_uuid(); fw uuid:=gen_random_uuid(); fs uuid:=gen_random_uuid(); fo uuid:=gen_random_uuid(); BEGIN
  INSERT INTO deployment_prices(id,deployment_id,input_token_limit,output_token_limit,pricing_version,price_lines,batch_price_lines,max_units) VALUES(bp,d,100,10,3,'[{"meter":"input_tokens","microusd_per_batch":"100000","batch":1000000,"unit_label":"/M tokens","sku_label":"Input"}]','[{"meter":"input_tokens","microusd_per_batch":"50000","batch":1000000,"unit_label":"/M tokens","sku_label":"Input (batch)"}]','{}');
  BEGIN INSERT INTO deployment_prices(id,deployment_id,input_microusd_per_million,output_microusd_per_million,input_token_limit,output_token_limit,pricing_version,batch_price_lines) VALUES(gen_random_uuid(),d,1,1,100,10,1,'[]'); RAISE EXCEPTION 'v1 batch prices allowed'; EXCEPTION WHEN check_violation THEN NULL; END;
  BEGIN UPDATE deployment_prices SET batch_price_lines=NULL WHERE id=bp; RAISE EXCEPTION 'batch price rewrite allowed'; EXCEPTION WHEN insufficient_privilege THEN NULL; END;
  INSERT INTO stored_files(id,object_key,purpose,workspace_id,created_by_api_key_id,backend,encryption_key_id,api_purpose) VALUES(fin,'batch_input/'||ws||'/'||fin,'batch_input',ws,k,'s3','k2026','batch');
  INSERT INTO stored_files(id,object_key,purpose,workspace_id,created_by_api_key_id,backend,encryption_key_id) VALUES(fw,'batch_output/'||ws||'/'||fw,'batch_output',ws,k,'s3','k2026');
  INSERT INTO stored_files(id,object_key,purpose,workspace_id,created_by_api_key_id,backend,encryption_key_id) VALUES(fs,'batch_output/'||ws||'/'||fs,'batch_output',ws,k,'s3','k2026');
  INSERT INTO stored_files(id,object_key,purpose,workspace_id,created_by_api_key_id,backend,encryption_key_id,api_purpose) VALUES(fo,'batch_output/'||ws||'/'||fo,'batch_output',ws,k,'s3','k2026','batch_output');
  INSERT INTO inference_executions(id,workspace_id,api_key_id,deployment_id,public_model,provider,streamed,state,root_request_id,workload_kind) VALUES(env,ws,k,d,'mixed','mixed',false,'started',env,'batches');
  INSERT INTO governance_reservations(execution_id,workspace_id,api_key_id,deployment_id,admitted_at,minute_start,month_start,lease_expires_at,state,reserved_tokens,held_microusd,request_count,price_tier) VALUES(env,ws,k,d,now(),date_trunc('minute',now()),date_trunc('month',now()),now()+interval '26 hours','pending',220,10,2,'standard');
  INSERT INTO async_jobs(id,kind,workspace_id,api_key_id,deployment_id,execution_id,public_model,provider,poll_deadline_at,batch_endpoint,batch_mode,user_id,input_file_id,work_file_id,request_total,request_completed,request_failed,upstream_status) VALUES(bj,'batch',ws,k,d,env,'mixed','mixed',now()+interval '26 hours','/v1/responses','gateway',u,fin,fw,2,0,0,'validating');
  UPDATE async_jobs SET runner_id=gen_random_uuid(),runner_lease_until=clock_timestamp()+interval '60 seconds' WHERE id IN(SELECT id FROM async_jobs WHERE batch_mode='gateway' AND settled_at IS NULL AND (runner_lease_until IS NULL OR runner_lease_until<clock_timestamp()) LIMIT 1 FOR UPDATE SKIP LOCKED);
  UPDATE async_jobs SET state='in_progress',upstream_status='in_progress',in_progress_at=clock_timestamp(),last_progress_at=clock_timestamp() WHERE id=bj AND state='queued';
  INSERT INTO batch_lines(job_id,workspace_id,line_no,state,execution_id) VALUES(bj,ws,0,'running',line) ON CONFLICT DO NOTHING;
  PERFORM r.state,r.held_microusd FROM governance_reservations r JOIN async_jobs j ON j.execution_id=r.execution_id WHERE r.execution_id=env AND j.id=bj AND j.batch_mode='gateway' FOR UPDATE OF r;
  INSERT INTO inference_executions(id,workspace_id,api_key_id,deployment_id,public_model,provider,streamed,state,root_request_id,batch_job_id) VALUES(line,ws,k,d,'rollback','openai_compatible',false,'started',line,bj);
  INSERT INTO governance_reservations(execution_id,workspace_id,api_key_id,deployment_id,price_id,admitted_at,minute_start,month_start,lease_expires_at,state,reserved_tokens,held_microusd) VALUES(line,ws,k,d,bp,now(),date_trunc('minute',now()),date_trunc('month',now()),now()+interval '2 minutes','pending',110,5);
  UPDATE governance_reservations SET held_microusd=held_microusd-5 WHERE execution_id=env AND state='pending' AND held_microusd>=5;
  UPDATE batch_lines SET state='failed',status_code=429,error_code='rate_limit_error',finished_at=clock_timestamp() WHERE job_id=bj AND line_no=0 AND state='running';
  UPDATE batch_lines SET state='running',attempts=attempts+1,execution_id=gen_random_uuid(),status_code=NULL,error_code=NULL,finished_at=NULL WHERE job_id=bj AND line_no=0 AND state='failed' AND attempts=1 AND segment IS NULL;
  UPDATE batch_lines SET state='succeeded',status_code=200,finished_at=clock_timestamp() WHERE job_id=bj AND line_no=0 AND state='running';
  UPDATE async_jobs SET request_completed=coalesce(request_completed,0)+1,request_failed=coalesce(request_failed,0)+0,last_progress_at=clock_timestamp() WHERE id=bj;
  INSERT INTO batch_segments(job_id,workspace_id,seq,file_id,lines) VALUES(bj,ws,0,fs,1);
  UPDATE batch_lines SET segment=0 WHERE job_id=bj AND line_no=ANY(ARRAY[0]) AND segment IS NULL AND state<>'running';
  UPDATE batch_lines SET state='interrupted',error_code='interrupted',finished_at=clock_timestamp() WHERE job_id=bj AND state='running';
  PERFORM seq,file_id FROM batch_segments WHERE job_id=bj ORDER BY seq;
  PERFORM coalesce(sum(r.actual_microusd) FILTER(WHERE r.state='settled'),0) FROM governance_reservations r WHERE r.execution_id IN (SELECT e2.id FROM inference_executions e2 WHERE e2.batch_job_id=bj);
  BEGIN UPDATE batch_lines SET state='running' WHERE job_id=bj AND line_no=0; RAISE EXCEPTION 'finished batch line restarted'; EXCEPTION WHEN raise_exception THEN IF SQLERRM NOT LIKE 'invalid batch line transition%' THEN RAISE; END IF; END;
  BEGIN UPDATE batch_lines SET segment=1 WHERE job_id=bj AND line_no=0; RAISE EXCEPTION 'batch line segment rewritten'; EXCEPTION WHEN raise_exception THEN IF SQLERRM<>'batch line identity is immutable' THEN RAISE; END IF; END;
  BEGIN DELETE FROM batch_lines WHERE job_id=bj; RAISE EXCEPTION 'batch line removal allowed'; EXCEPTION WHEN insufficient_privilege THEN NULL; END;
  BEGIN UPDATE batch_segments SET lines=2 WHERE job_id=bj; RAISE EXCEPTION 'batch segment rewrite allowed'; EXCEPTION WHEN insufficient_privilege THEN NULL; END;
  BEGIN UPDATE async_jobs SET batch_mode='native' WHERE id=bj; RAISE EXCEPTION 'batch mode rewrite allowed'; EXCEPTION WHEN insufficient_privilege THEN NULL; END;
  UPDATE async_jobs SET state='completed',upstream_status='completed',output_file_id=coalesce(output_file_id,fo),completed_at=coalesce(completed_at,clock_timestamp()),runner_lease_until=NULL WHERE id=bj AND state IN('queued','in_progress');
  BEGIN UPDATE async_jobs SET output_file_id=fin WHERE id=bj; RAISE EXCEPTION 'batch output rewrite allowed'; EXCEPTION WHEN raise_exception THEN IF SQLERRM<>'async job identity is immutable' THEN RAISE; END IF; END;
  UPDATE governance_reservations SET state='settled',actual_microusd=0,input_tokens=0,output_tokens=0 WHERE execution_id=env AND state='pending';
  UPDATE inference_executions SET state='succeeded',input_tokens=0,output_tokens=0,elapsed_ms=1,completed_at=clock_timestamp(),finish_reason='stop' WHERE id=env AND state='started' AND workload_kind='batches';
  INSERT INTO monetary_ledger(id,execution_id,kind,amount_microusd,input_tokens,output_tokens) VALUES(gen_random_uuid(),env,'settlement',0,0,0);
  UPDATE async_jobs SET settled_at=clock_timestamp() WHERE id=bj AND settled_at IS NULL;
  -- Native: created before submission, upstream id written once, batch tier pinned.
  INSERT INTO inference_executions(id,workspace_id,api_key_id,deployment_id,public_model,provider,streamed,state,root_request_id,workload_kind) VALUES(nenv,ws,k,d,'rollback','openai',false,'started',nenv,'batches');
  INSERT INTO governance_reservations(execution_id,workspace_id,api_key_id,deployment_id,price_id,admitted_at,minute_start,month_start,lease_expires_at,state,reserved_tokens,held_microusd,request_count,price_tier) VALUES(nenv,ws,k,d,bp,now(),date_trunc('minute',now()),date_trunc('month',now()),now()+interval '26 hours','pending',110,1,1,'batch');
  INSERT INTO async_jobs(id,kind,workspace_id,api_key_id,deployment_id,execution_id,public_model,provider,poll_deadline_at,batch_endpoint,batch_mode,input_file_id,work_file_id,price_tier,request_total,request_completed,request_failed) VALUES(nj,'batch',ws,k,d,nenv,'rollback','openai',now()+interval '26 hours','/v1/embeddings','native',fin,fw,'batch',1,0,0);
  UPDATE async_jobs SET submit_started_at=clock_timestamp() WHERE id=nj AND submit_started_at IS NULL AND upstream_id IS NULL AND cancel_requested_at IS NULL AND state='queued';
  UPDATE async_jobs SET upstream_id='batch_native_probe' WHERE id=nj AND upstream_id IS NULL;
  BEGIN UPDATE async_jobs SET upstream_id='batch_other' WHERE id=nj; RAISE EXCEPTION 'native upstream id rewrite allowed'; EXCEPTION WHEN raise_exception THEN IF SQLERRM<>'async job identity is immutable' THEN RAISE; END IF; END;
  BEGIN INSERT INTO governance_reservations(execution_id,workspace_id,api_key_id,deployment_id,admitted_at,minute_start,month_start,lease_expires_at,state,price_tier) VALUES(gen_random_uuid(),ws,k,d,now(),now(),now(),now(),'pending','batch'); RAISE EXCEPTION 'unpinned batch tier allowed'; EXCEPTION WHEN check_violation OR foreign_key_violation THEN NULL; END;
  BEGIN INSERT INTO async_jobs(id,kind,workspace_id,api_key_id,deployment_id,execution_id,public_model,provider,upstream_id,poll_deadline_at,batch_endpoint,batch_mode,input_file_id,work_file_id) VALUES(gen_random_uuid(),'batch',ws,k,d,gen_random_uuid(),'x','x','batch_y',now(),'/v1/chat/completions','gateway',fin,fw); RAISE EXCEPTION 'gateway batch with upstream id allowed'; EXCEPTION WHEN check_violation OR foreign_key_violation THEN NULL; END;
  PERFORM coalesce(batch_mode,'native'),count(*) FROM async_jobs WHERE kind='batch' AND settled_at IS NULL GROUP BY 1;
 END;
 -- Batch scheduling (0022): route settings, gate signal, the per-route claim
 -- lock and fair-share order, demand heartbeat, the stall clock and reads.
 DECLARE sj uuid:=gen_random_uuid(); senv uuid:=gen_random_uuid(); sin uuid:=gen_random_uuid(); swk uuid:=gen_random_uuid(); BEGIN
  INSERT INTO deployment_batch_scheduling(deployment_id,max_concurrency,yield_live_threshold,metrics_url,metrics_max_waiting,metrics_max_running,metrics_max_kv_cache_percent,priority,window_timezone,window_days,window_start_minute,window_end_minute,updated_by) VALUES(d,2,1,'http://127.0.0.1:19091/metrics',0,NULL,90,10,'America/New_York',31,1140,420,u) ON CONFLICT(deployment_id) DO UPDATE SET max_concurrency=excluded.max_concurrency,yield_live_threshold=excluded.yield_live_threshold,metrics_url=excluded.metrics_url,metrics_max_waiting=excluded.metrics_max_waiting,metrics_max_running=excluded.metrics_max_running,metrics_max_kv_cache_percent=excluded.metrics_max_kv_cache_percent,priority=excluded.priority,window_timezone=excluded.window_timezone,window_days=excluded.window_days,window_start_minute=excluded.window_start_minute,window_end_minute=excluded.window_end_minute,updated_at=clock_timestamp(),updated_by=excluded.updated_by;
  UPDATE deployment_batch_scheduling SET max_concurrency=3,updated_at=clock_timestamp() WHERE deployment_id=d;
  BEGIN UPDATE deployment_batch_scheduling SET metrics_max_waiting=NULL,metrics_max_kv_cache_percent=NULL WHERE deployment_id=d; RAISE EXCEPTION 'metrics without threshold allowed'; EXCEPTION WHEN check_violation THEN NULL; END;
  BEGIN UPDATE deployment_batch_scheduling SET window_days=NULL WHERE deployment_id=d; RAISE EXCEPTION 'partial window allowed'; EXCEPTION WHEN check_violation THEN NULL; END;
  BEGIN UPDATE deployment_batch_scheduling SET priority=-1 WHERE deployment_id=d; RAISE EXCEPTION 'priority ahead of live traffic allowed'; EXCEPTION WHEN check_violation THEN NULL; END;
  BEGIN DELETE FROM deployment_batch_scheduling WHERE deployment_id=d; RAISE EXCEPTION 'batch scheduling removable'; EXCEPTION WHEN insufficient_privilege THEN NULL; END;
  INSERT INTO deployment_batch_signals(deployment_id,paused_reason,checked_at,live_in_flight,metrics_checked_at,metrics_ok,metrics_waiting,metrics_running,metrics_kv_cache_permille,metrics_error) VALUES(d,'server_busy',clock_timestamp(),0,clock_timestamp(),true,1,2,913,NULL) ON CONFLICT(deployment_id) DO UPDATE SET paused_reason=excluded.paused_reason,checked_at=excluded.checked_at,live_in_flight=excluded.live_in_flight,metrics_checked_at=excluded.metrics_checked_at,metrics_ok=excluded.metrics_ok,metrics_waiting=excluded.metrics_waiting,metrics_running=excluded.metrics_running,metrics_kv_cache_permille=excluded.metrics_kv_cache_permille,metrics_error=excluded.metrics_error;
  PERFORM count(*) FROM inference_executions e JOIN governance_reservations r ON r.execution_id=e.id WHERE e.deployment_id=d AND e.state='started' AND e.batch_job_id IS NULL AND e.workload_kind NOT IN('batches','videos') AND r.state='pending' AND r.lease_expires_at>clock_timestamp();
  INSERT INTO stored_files(id,object_key,purpose,workspace_id,created_by_api_key_id,backend,encryption_key_id,api_purpose) VALUES(sin,'batch_input/'||ws||'/'||sin,'batch_input',ws,k,'s3','k2026','batch');
  INSERT INTO stored_files(id,object_key,purpose,workspace_id,created_by_api_key_id,backend,encryption_key_id) VALUES(swk,'batch_output/'||ws||'/'||swk,'batch_output',ws,k,'s3','k2026');
  INSERT INTO inference_executions(id,workspace_id,api_key_id,deployment_id,public_model,provider,streamed,state,root_request_id,workload_kind) VALUES(senv,ws,k,d,'mixed','mixed',false,'started',senv,'batches');
  INSERT INTO governance_reservations(execution_id,workspace_id,api_key_id,deployment_id,admitted_at,minute_start,month_start,lease_expires_at,state,reserved_tokens,held_microusd,request_count,price_tier) VALUES(senv,ws,k,d,now(),date_trunc('minute',now()),date_trunc('month',now()),now()+interval '50 hours','pending',110,5,1,'standard');
  INSERT INTO async_jobs(id,kind,workspace_id,api_key_id,deployment_id,execution_id,public_model,provider,poll_deadline_at,batch_endpoint,batch_mode,user_id,input_file_id,work_file_id,request_total,request_completed,request_failed,upstream_status,completion_window_hours) VALUES(sj,'batch',ws,k,d,senv,'mixed','mixed',now()+interval '50 hours','/v1/chat/completions','gateway',u,sin,swk,1,0,0,'validating',48);
  BEGIN UPDATE async_jobs SET completion_window_hours=24 WHERE id=sj; RAISE EXCEPTION 'batch window rewrite allowed'; EXCEPTION WHEN insufficient_privilege THEN NULL; END;
  PERFORM pg_advisory_xact_lock(hashtextextended('omg_batch_route:'||d::text,0));
  PERFORM count(*) FROM batch_lines WHERE deployment_id=d AND state='running';
  INSERT INTO batch_route_waits(job_id,workspace_id,deployment_id,waiting_lines,ready) VALUES(sj,ws,d,1,true) ON CONFLICT(deployment_id,job_id) DO UPDATE SET ready=true,updated_at=clock_timestamp();
  PERFORM job_id FROM (SELECT w.job_id,coalesce(jr.n,0) jn,max(w.last_claim_at) OVER (PARTITION BY w.workspace_id) wl,w.last_claim_at,w.since FROM batch_route_waits w LEFT JOIN (SELECT job_id,count(*) n FROM batch_lines WHERE deployment_id=d AND state='running' GROUP BY job_id) jr ON jr.job_id=w.job_id WHERE w.deployment_id=d AND w.ready AND w.updated_at>clock_timestamp()-make_interval(secs=>10)) x ORDER BY jn,wl NULLS FIRST,last_claim_at NULLS FIRST,since,job_id LIMIT 1;
  INSERT INTO batch_lines(job_id,workspace_id,line_no,state,execution_id,deployment_id) VALUES(sj,ws,0,'running',gen_random_uuid(),d) ON CONFLICT DO NOTHING;
  UPDATE batch_route_waits SET last_claim_at=clock_timestamp() WHERE deployment_id=d AND job_id=sj;
  BEGIN UPDATE batch_lines SET deployment_id=NULL WHERE job_id=sj; RAISE EXCEPTION 'batch line route rewrite allowed'; EXCEPTION WHEN insufficient_privilege THEN NULL; END;
  BEGIN UPDATE batch_route_waits SET deployment_id=gen_random_uuid() WHERE job_id=sj; RAISE EXCEPTION 'batch demand re-keyed'; EXCEPTION WHEN insufficient_privilege THEN NULL; END;
  DELETE FROM batch_route_waits WHERE job_id=sj AND deployment_id<>ALL(ARRAY[d]);
  INSERT INTO batch_route_waits(job_id,workspace_id,deployment_id,waiting_lines,reason,ready) SELECT sj,ws,x,n,r,y FROM unnest(ARRAY[d],ARRAY[3],ARRAY['outside_window'],ARRAY[false]) AS v(x,n,r,y) ON CONFLICT(deployment_id,job_id) DO UPDATE SET waiting_lines=excluded.waiting_lines,reason=excluded.reason,ready=excluded.ready,updated_at=clock_timestamp();
  UPDATE async_jobs SET last_waited_at=clock_timestamp() WHERE id=sj AND state IN('queued','in_progress');
  UPDATE batch_lines SET state='failed',status_code=429,error_code='rate_limit_error',finished_at=clock_timestamp() WHERE job_id=sj AND line_no=0 AND state='running';
  UPDATE batch_lines SET state='running',attempts=attempts+1,execution_id=gen_random_uuid(),status_code=NULL,error_code=NULL,finished_at=NULL WHERE job_id=sj AND line_no=0 AND state='failed' AND attempts=1 AND segment IS NULL;
  PERFORM coalesce(sum(waiting_lines),0),count(*) FROM batch_route_waits WHERE deployment_id=d AND updated_at>clock_timestamp()-make_interval(secs=>10);
  PERFORM s.paused_reason,s.metrics_kv_cache_permille FROM (SELECT 1) one LEFT JOIN deployment_batch_signals s ON s.deployment_id=d;
  PERFORM count(*) FROM batch_lines l WHERE l.job_id=sj AND l.state='running';
  DELETE FROM batch_route_waits WHERE job_id=sj;
 END;
END $$;
ROLLBACK;
