-- Change notifications for per-replica caches (scale plan P4).
-- Explicit operator upgrade only (`migrate`); never applied implicitly by serve.
--
-- Every replica caches what it reads *before* admission (key metadata by
-- token hash, candidate routes, routing configuration, immutable prices).
-- Admission never trusts a cache: it re-checks authorization, catalog
-- eligibility and policies live under its scope locks (0027). The caches only
-- need to learn quickly that something changed:
--   * config_versions holds one monotonically increasing version per domain
--     (topic): catalog (models, deployments, providers, prices, routing,
--     catalogs, type catalogs), access (memberships, platform grants, user and
--     workspace state, service accounts, workspace entitlements), keys (key
--     state and key model restrictions), policy (limits and budgets) and
--     settings (installation settings);
--   * row triggers on those tables bump their topic once per transaction (at
--     commit) and NOTIFY channel omg_config with payload '<topic>:<version>'
--     (delivered after commit, so a listener never sees an uncommitted change);
--   * replicas LISTEN on a dedicated session connection and also poll the
--     versions every second; a cache entry is valid only while the versions it
--     was loaded under are current, and caches are bypassed (live reads) when
--     the versions cannot be confirmed for 3 seconds.
-- Inserts that only widen authorization (new users, workspaces, keys,
-- service accounts, role and membership grants) do not bump: caches never hold
-- negative authorization results. Updates bump only when a column that
-- authorization or routing reads changes, so sign-in's no-op upserts do not
-- flush every replica's caches.
--
-- Concurrency: the triggers are deferred constraint triggers, so the bump
-- runs at commit, after every other lock the transaction takes (including
-- authority locks that 0027's triggers acquire lazily). The first bump of a
-- transaction locks every config_versions row in topic order, so concurrent
-- configuration writers serialize there instead of deadlocking (they
-- already serialize on the installation row or the exclusive catalog lock).
-- Admission, settlement and background accounting never write these tables
-- and never notify.
--
-- No existing table, column or history row changes.

CREATE TABLE config_versions(
 topic text PRIMARY KEY CHECK(topic IN ('access','catalog','keys','policy','settings')),
 version bigint NOT NULL DEFAULT 0 CHECK(version>=0),
 changed_at timestamptz
);
INSERT INTO config_versions(topic) VALUES('access'),('catalog'),('keys'),('policy'),('settings');

-- Versions never move backwards and topics are never re-keyed or removed
-- (a lower version could make a stale cache entry look current again).
CREATE FUNCTION config_versions_guard() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 IF TG_OP IN ('DELETE','TRUNCATE') THEN
  RAISE EXCEPTION 'configuration versions are never removed' USING ERRCODE='23514';
 END IF;
 IF NEW.topic<>OLD.topic OR NEW.version<OLD.version THEN
  RAISE EXCEPTION 'configuration versions only move forward' USING ERRCODE='23514';
 END IF;
 RETURN NEW;
END $$;
CREATE TRIGGER config_versions_forward BEFORE UPDATE OR DELETE ON config_versions
 FOR EACH ROW EXECUTE FUNCTION config_versions_guard();
CREATE TRIGGER config_versions_no_truncate BEFORE TRUNCATE ON config_versions
 FOR EACH STATEMENT EXECUTE FUNCTION config_versions_guard();

-- Bump `topic` once per transaction and notify its new version at commit.
CREATE FUNCTION omg_config_bump(changed text) RETURNS void LANGUAGE plpgsql AS $$
DECLARE
 seen text := coalesce(current_setting('omg.config_bumped',true),'');
 v bigint;
BEGIN
 IF strpos(seen,'|'||changed||'|')>0 THEN
  RETURN;
 END IF;
 IF seen='' THEN
  -- First bump of this transaction: every topic row, in canonical order.
  PERFORM 1 FROM config_versions c ORDER BY c.topic FOR NO KEY UPDATE;
 END IF;
 UPDATE config_versions c SET version=c.version+1,changed_at=clock_timestamp() WHERE c.topic=changed
  RETURNING c.version INTO v;
 IF v IS NULL THEN
  RAISE EXCEPTION 'unknown configuration topic %',changed;
 END IF;
 PERFORM pg_notify('omg_config',changed||':'||v);
 PERFORM set_config('omg.config_bumped',CASE WHEN seen='' THEN '|' ELSE seen END||changed||'|',true);
END $$;

CREATE FUNCTION omg_config_changed() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 PERFORM omg_config_bump(TG_ARGV[0]);
 RETURN NULL;
END $$;

-- catalog: global catalog and routing configuration (every change).
CREATE CONSTRAINT TRIGGER omg_config_models AFTER INSERT OR UPDATE OR DELETE ON models
 DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION omg_config_changed('catalog');
CREATE CONSTRAINT TRIGGER omg_config_deployments AFTER INSERT OR UPDATE OR DELETE ON deployments
 DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION omg_config_changed('catalog');
CREATE CONSTRAINT TRIGGER omg_config_provider_connections AFTER INSERT OR UPDATE OR DELETE ON provider_connections
 DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION omg_config_changed('catalog');
CREATE CONSTRAINT TRIGGER omg_config_deployment_prices AFTER INSERT ON deployment_prices
 DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION omg_config_changed('catalog');
CREATE CONSTRAINT TRIGGER omg_config_routing_policies AFTER INSERT OR UPDATE OR DELETE ON routing_policies
 DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION omg_config_changed('catalog');
CREATE CONSTRAINT TRIGGER omg_config_deployment_routing AFTER INSERT OR UPDATE OR DELETE ON deployment_routing
 DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION omg_config_changed('catalog');
CREATE CONSTRAINT TRIGGER omg_config_catalogs AFTER INSERT OR UPDATE OR DELETE ON catalogs
 DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION omg_config_changed('catalog');
CREATE CONSTRAINT TRIGGER omg_config_catalog_models AFTER INSERT OR UPDATE OR DELETE ON catalog_models
 DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION omg_config_changed('catalog');
CREATE CONSTRAINT TRIGGER omg_config_type_catalogs AFTER INSERT OR UPDATE OR DELETE ON workspace_type_catalogs
 DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION omg_config_changed('catalog');

-- access: who may use a workspace, and which models it is entitled to.
CREATE CONSTRAINT TRIGGER omg_config_users AFTER UPDATE ON users DEFERRABLE INITIALLY DEFERRED FOR EACH ROW
 WHEN (OLD.disabled_at IS DISTINCT FROM NEW.disabled_at OR OLD.cleaned_at IS DISTINCT FROM NEW.cleaned_at)
 EXECUTE FUNCTION omg_config_changed('access');
CREATE CONSTRAINT TRIGGER omg_config_users_delete AFTER DELETE ON users
 DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION omg_config_changed('access');
CREATE CONSTRAINT TRIGGER omg_config_platform_grants AFTER UPDATE OR DELETE ON platform_role_grants
 DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION omg_config_changed('access');
CREATE CONSTRAINT TRIGGER omg_config_memberships AFTER UPDATE OR DELETE ON workspace_membership_grants
 DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION omg_config_changed('access');
CREATE CONSTRAINT TRIGGER omg_config_workspaces AFTER UPDATE ON workspaces DEFERRABLE INITIALLY DEFERRED FOR EACH ROW
 WHEN (OLD.disabled_at IS DISTINCT FROM NEW.disabled_at OR OLD.owner_user_id IS DISTINCT FROM NEW.owner_user_id OR OLD.kind IS DISTINCT FROM NEW.kind)
 EXECUTE FUNCTION omg_config_changed('access');
CREATE CONSTRAINT TRIGGER omg_config_workspaces_delete AFTER DELETE ON workspaces
 DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION omg_config_changed('access');
CREATE CONSTRAINT TRIGGER omg_config_service_accounts AFTER UPDATE ON service_accounts DEFERRABLE INITIALLY DEFERRED FOR EACH ROW
 WHEN (OLD.disabled_at IS DISTINCT FROM NEW.disabled_at OR OLD.workspace_id IS DISTINCT FROM NEW.workspace_id)
 EXECUTE FUNCTION omg_config_changed('access');
CREATE CONSTRAINT TRIGGER omg_config_service_accounts_delete AFTER DELETE ON service_accounts
 DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION omg_config_changed('access');
CREATE CONSTRAINT TRIGGER omg_config_model_grants AFTER INSERT OR UPDATE OR DELETE ON workspace_model_grants
 DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION omg_config_changed('access');
CREATE CONSTRAINT TRIGGER omg_config_catalog_overrides AFTER INSERT OR UPDATE OR DELETE ON workspace_catalog_overrides
 DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION omg_config_changed('access');
CREATE CONSTRAINT TRIGGER omg_config_catalog_override_items AFTER INSERT OR UPDATE OR DELETE ON workspace_catalog_override_items
 DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION omg_config_changed('access');

-- keys: key state and per-key model restrictions.
CREATE CONSTRAINT TRIGGER omg_config_api_keys AFTER UPDATE ON api_keys DEFERRABLE INITIALLY DEFERRED FOR EACH ROW
 WHEN (OLD.revoked_at IS DISTINCT FROM NEW.revoked_at OR OLD.disabled_at IS DISTINCT FROM NEW.disabled_at
  OR OLD.expires_at IS DISTINCT FROM NEW.expires_at OR OLD.workspace_id IS DISTINCT FROM NEW.workspace_id
  OR OLD.issued_to_user_id IS DISTINCT FROM NEW.issued_to_user_id OR OLD.service_account_id IS DISTINCT FROM NEW.service_account_id
  OR OLD.governance_key_id IS DISTINCT FROM NEW.governance_key_id OR OLD.secret_hash IS DISTINCT FROM NEW.secret_hash)
 EXECUTE FUNCTION omg_config_changed('keys');
CREATE CONSTRAINT TRIGGER omg_config_api_keys_delete AFTER DELETE ON api_keys
 DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION omg_config_changed('keys');
CREATE CONSTRAINT TRIGGER omg_config_key_restrictions AFTER INSERT OR UPDATE OR DELETE ON key_model_restrictions
 DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION omg_config_changed('keys');
CREATE CONSTRAINT TRIGGER omg_config_key_selections AFTER INSERT OR UPDATE OR DELETE ON key_model_selections
 DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION omg_config_changed('keys');

-- policy: limits and budgets (admission always reads these live).
CREATE CONSTRAINT TRIGGER omg_config_type_policies AFTER INSERT OR UPDATE OR DELETE ON workspace_type_policies
 DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION omg_config_changed('policy');
CREATE CONSTRAINT TRIGGER omg_config_policy_overrides AFTER INSERT OR UPDATE OR DELETE ON workspace_platform_policy_overrides
 DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION omg_config_changed('policy');
CREATE CONSTRAINT TRIGGER omg_config_local_policies AFTER INSERT OR UPDATE OR DELETE ON workspace_local_policies
 DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION omg_config_changed('policy');
CREATE CONSTRAINT TRIGGER omg_config_key_policies AFTER INSERT OR UPDATE OR DELETE ON key_policies
 DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION omg_config_changed('policy');
CREATE CONSTRAINT TRIGGER omg_config_policy_budgets AFTER INSERT OR UPDATE OR DELETE ON policy_budgets
 DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION omg_config_changed('policy');

-- settings: installation settings (one seeded row).
CREATE CONSTRAINT TRIGGER omg_config_installation_settings AFTER UPDATE ON installation_settings
 DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION omg_config_changed('settings');
