-- Read-only ACL assertions, then rollback-only statements as the runtime role.
DO $$ DECLARE r record; t text; BEGIN
  SELECT * INTO STRICT r FROM pg_roles WHERE rolname='gateway_runtime';
  IF r.rolsuper OR r.rolcreatedb OR r.rolcreaterole OR r.rolreplication OR r.rolbypassrls THEN
    RAISE EXCEPTION 'runtime has privileged role attributes';
  END IF;
  IF EXISTS (SELECT FROM pg_auth_members WHERE member=r.oid) THEN RAISE EXCEPTION 'runtime has unexpected role memberships'; END IF;
  IF EXISTS (SELECT FROM pg_database WHERE datname=current_database() AND datdba=r.oid) OR
     EXISTS (SELECT FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname='public' AND c.relowner=r.oid) THEN
    RAISE EXCEPTION 'runtime owns database objects';
  END IF;
  IF has_database_privilege('gateway_runtime','postgres','CONNECT') OR has_database_privilege('gateway_runtime','template1','CONNECT') THEN
    RAISE EXCEPTION 'runtime can connect to maintenance databases';
  END IF;
  IF has_schema_privilege('gateway_runtime','public','CREATE') OR
     has_database_privilege('gateway_runtime',current_database(),'CREATE') OR
     has_database_privilege('gateway_runtime',current_database(),'TEMP') THEN RAISE EXCEPTION 'runtime can create objects'; END IF;
  IF has_column_privilege('gateway_runtime','public.users','platform_admin','INSERT') OR
     has_column_privilege('gateway_runtime','public.users','platform_admin','UPDATE') OR
     has_column_privilege('gateway_runtime','public.users','disabled_at','UPDATE') THEN RAISE EXCEPTION 'runtime can provision privileged identities'; END IF;
  FOREACH t IN ARRAY ARRAY['_sqlx_migrations','deployment_prices','monetary_ledger','audit_events'] LOOP
    IF has_table_privilege('gateway_runtime','public.'||t,'UPDATE,DELETE,TRUNCATE') OR
       has_any_column_privilege('gateway_runtime','public.'||t,'UPDATE') THEN RAISE EXCEPTION 'runtime can mutate immutable history: %',t; END IF;
  END LOOP;
  IF has_table_privilege('gateway_runtime','public.key_model_restrictions','UPDATE,DELETE,TRUNCATE') OR
     has_any_column_privilege('gateway_runtime','public.key_model_restrictions','UPDATE') OR
     has_table_privilege('gateway_runtime','public.key_model_selections','UPDATE,DELETE,TRUNCATE') OR
     has_any_column_privilege('gateway_runtime','public.key_model_selections','UPDATE') THEN
    RAISE EXCEPTION 'runtime can rewrite key model restrictions';
  END IF;
  IF has_table_privilege('gateway_runtime','public._sqlx_migrations','INSERT') THEN RAISE EXCEPTION 'runtime can write migrations'; END IF;
  FOR t IN SELECT tablename FROM pg_tables WHERE schemaname='public' AND tablename <> '_sqlx_migrations' LOOP
    IF NOT has_table_privilege('gateway_runtime','public.'||t,'SELECT') THEN RAISE EXCEPTION 'review privileges for new table: %',t; END IF;
  END LOOP;
  IF EXISTS (SELECT FROM pg_proc p JOIN pg_namespace n ON n.oid=p.pronamespace WHERE n.nspname='public' AND has_function_privilege('gateway_runtime',p.oid,'EXECUTE')) THEN RAISE EXCEPTION 'unexpected direct public function execution'; END IF;
END $$;
BEGIN;
SET LOCAL ROLE gateway_runtime;
SELECT version, checksum, success FROM public._sqlx_migrations ORDER BY version;
SELECT model_id FROM public.workspace_model_grants WHERE false FOR SHARE;
SELECT model_id FROM public.user_model_grants WHERE false FOR SHARE;
SELECT id FROM public.users WHERE false FOR UPDATE;
-- Direct catalog records and pre-pagination search require no new privileges.
SELECT id,name,provider,endpoint,region,enabled FROM public.provider_connections
  WHERE false AND strpos(lower(name),'probe') > 0 ORDER BY name,id LIMIT 1;
SELECT id,public_name,display_name,enabled FROM public.models
  WHERE false AND strpos(lower(public_name),'probe') > 0 ORDER BY public_name,id LIMIT 1;
SELECT id,model_id,provider_connection_id,upstream_model,enabled FROM public.deployments
  WHERE false AND strpos(lower(upstream_model),'probe') > 0 ORDER BY created_at,id LIMIT 1;
-- Exercise invoker triggers and real grant-row locks with rollback-only data.
DO $$ DECLARE
  u uuid := gen_random_uuid(); o uuid := gen_random_uuid();
  shared uuid := gen_random_uuid(); personal uuid := gen_random_uuid();
  k uuid := gen_random_uuid(); m uuid := gen_random_uuid();
BEGIN
  INSERT INTO public.users(id,email) VALUES (u,'staging-probe-'||u||'@example.invalid');
  INSERT INTO public.organizations(id,slug,name) VALUES (o,'staging-probe-'||o,'Rollback-only probe');
  INSERT INTO public.organization_memberships(organization_id,user_id,role) VALUES(o,u,'owner');
  INSERT INTO public.workspaces(id,organization_id,name,kind) VALUES(shared,o,'Probe project','project');
  INSERT INTO public.workspaces(id,organization_id,name,kind,owner_user_id) VALUES(personal,o,'Probe personal','personal',u);
  INSERT INTO public.workspace_memberships(organization_id,workspace_id,user_id,role) VALUES(o,personal,u,'owner'),(o,shared,u,'owner');
  -- /me's additive metadata reads these existing columns and keeps the live
  -- user FOR SHARE lock. No additional runtime privileges are needed.
  PERFORM platform_admin FROM public.users WHERE id=u AND disabled_at IS NULL FOR SHARE;
  IF NOT EXISTS (
    SELECT org.id,org.name,org.slug,om.role,ws.id,ws.organization_id,ws.name,ws.kind,ws.owner_user_id,wm.role
    FROM public.organizations org
    JOIN public.workspaces ws ON ws.organization_id=org.id AND ws.disabled_at IS NULL
    LEFT JOIN public.organization_memberships om
      ON om.organization_id=org.id AND om.user_id=u AND om.disabled_at IS NULL
    LEFT JOIN public.workspace_memberships wm
      ON wm.organization_id=org.id AND wm.workspace_id=ws.id AND wm.user_id=u AND wm.disabled_at IS NULL
    WHERE org.id=o AND org.disabled_at IS NULL AND ws.id=shared AND om.role='owner' AND wm.role='owner'
  ) THEN RAISE EXCEPTION 'session membership projection failed'; END IF;
  INSERT INTO public.api_keys(id,organization_id,workspace_id,issued_to_user_id,name,secret_hash) VALUES(k,o,shared,u,'Never issued',decode(repeat('00',32),'hex'));
  IF (SELECT governance_key_id FROM public.api_keys WHERE id=k) <> k THEN RAISE EXCEPTION 'lineage trigger failed'; END IF;
  INSERT INTO public.models(id,public_name,display_name,enabled) VALUES(m,'probe-'||m,'Disabled probe',false);
  INSERT INTO public.organization_model_grants(organization_id,model_id,public_name) VALUES(o,m,'probe');
  INSERT INTO public.workspace_model_grants(organization_id,workspace_id,model_id) VALUES(o,shared,m);
  INSERT INTO public.user_model_grants(organization_id,user_id,model_id) VALUES(o,u,m);
  INSERT INTO public.key_model_restrictions(organization_id,workspace_id,governance_key_id) VALUES(o,shared,k);
  INSERT INTO public.key_model_selections(organization_id,workspace_id,governance_key_id,model_id) VALUES(o,shared,k,m);
  IF NOT EXISTS (SELECT FROM public.key_model_selections WHERE governance_key_id=k AND model_id=m) THEN RAISE EXCEPTION 'key model selection failed'; END IF;
  PERFORM model_id FROM public.workspace_model_grants WHERE organization_id=o FOR SHARE;
  PERFORM model_id FROM public.user_model_grants WHERE organization_id=o FOR SHARE;
  DELETE FROM public.workspace_model_grants WHERE organization_id=o AND model_id=m;
  DELETE FROM public.user_model_grants WHERE organization_id=o AND model_id=m;
  DELETE FROM public.organization_model_grants WHERE organization_id=o AND model_id=m;
  IF EXISTS (SELECT FROM public.key_model_selections WHERE governance_key_id=k) OR
     NOT EXISTS (SELECT FROM public.key_model_restrictions WHERE governance_key_id=k) THEN RAISE EXCEPTION 'key restriction cascade failed'; END IF;
  INSERT INTO public.organization_model_grants(organization_id,model_id,public_name) VALUES(o,m,'probe');
  IF EXISTS (SELECT FROM public.key_model_selections WHERE governance_key_id=k) THEN RAISE EXCEPTION 'key selection revived'; END IF;
  INSERT INTO public.audit_events(id,organization_id,actor_user_id,action) VALUES(gen_random_uuid(),o,u,'staging.rollback_probe');
  BEGIN
    INSERT INTO public.service_accounts(id,organization_id,workspace_id,name) VALUES(gen_random_uuid(),o,personal,'Must fail');
    RAISE EXCEPTION 'private workspace resource trigger failed';
  EXCEPTION WHEN check_violation THEN NULL; END;
END $$;
DO $$ BEGIN
  BEGIN
    EXECUTE 'CREATE TABLE public.runtime_must_not_create (id integer)';
    RAISE EXCEPTION 'runtime unexpectedly created a table';
  EXCEPTION WHEN insufficient_privilege THEN NULL; END;
  BEGIN
    EXECUTE 'UPDATE public.users SET platform_admin=true WHERE false';
    RAISE EXCEPTION 'runtime unexpectedly changed platform privileges';
  EXCEPTION WHEN insufficient_privilege THEN NULL; END;
  BEGIN
    EXECUTE 'DELETE FROM public.key_model_restrictions WHERE false';
    RAISE EXCEPTION 'runtime unexpectedly deleted restriction header';
  EXCEPTION WHEN insufficient_privilege THEN NULL; END;
  BEGIN
    EXECUTE 'UPDATE public.key_model_selections SET model_id=model_id WHERE false';
    RAISE EXCEPTION 'runtime unexpectedly rewrote selections';
  EXCEPTION WHEN insufficient_privilege THEN NULL; END;
  BEGIN
    EXECUTE 'DELETE FROM public.audit_events WHERE false';
    RAISE EXCEPTION 'runtime unexpectedly deleted audit history';
  EXCEPTION WHEN insufficient_privilege THEN NULL; END;
  BEGIN
    EXECUTE 'DELETE FROM public.monetary_ledger WHERE false';
    RAISE EXCEPTION 'runtime unexpectedly deleted financial history';
  EXCEPTION WHEN insufficient_privilege THEN NULL; END;
END $$;
ROLLBACK;
