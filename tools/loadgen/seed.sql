-- Load-test seeder: identities, keys, the mock catalog and policies.
-- Run only through `loadgen seed` (docs/operations.md "Capacity baseline").
-- THROWAWAY DATABASES ONLY: refuses any database not named omg_loadtest*.
-- Parameters arrive as session settings omg_seed.* (set by loadgen).
-- Keys are deterministic: token(k) = 'omg_' || hex32(sha256(seed||':key:'||k))
-- || '.' || hex(sha256(seed||':secret:'||k)); the generator derives the same
-- tokens, so none is ever printed or stored. Everything is set-based and
-- generated server-side (no data crosses the wire).
DO $$ BEGIN
  IF current_database() !~ '^omg_loadtest' THEN
    RAISE EXCEPTION 'loadgen seed refuses database %: only throwaway omg_loadtest* databases', current_database();
  END IF;
  IF EXISTS (SELECT 1 FROM models WHERE public_name = current_setting('omg_seed.model')) THEN
    RAISE EXCEPTION 'database already seeded (model % exists); recreate the throwaway database', current_setting('omg_seed.model');
  END IF;
END $$;

CREATE FUNCTION pg_temp.seed_id(label text) RETURNS uuid LANGUAGE sql STABLE AS
$$ SELECT left(encode(sha256(convert_to(current_setting('omg_seed.seed') || ':' || label, 'UTF8')), 'hex'), 32)::uuid $$;

CREATE TEMP TABLE seed_params AS SELECT
  current_setting('omg_seed.users')::int AS users,
  current_setting('omg_seed.shared')::int AS shared,
  current_setting('omg_seed.keys')::bigint AS keys;

-- Users, each with a User platform grant and a personal workspace.
INSERT INTO users(id, email)
  SELECT pg_temp.seed_id('user:' || i), 'loadtest-' || i || '@loadtest.invalid'
  FROM seed_params, generate_series(1, users) i;
-- User 1 is a platform Auditor (Auditor includes User entitlement): the
-- management reader of `loadgen run --readers` (platform reports, usage and
-- logs, plus its own workspaces).
INSERT INTO platform_role_grants(user_id, role, source)
  SELECT pg_temp.seed_id('user:' || i), CASE WHEN i = 1 THEN 'auditor' ELSE 'user' END, 'manual'
  FROM seed_params, generate_series(1, users) i;
-- Its browser session: cookie token hex(sha256(seed || ':session:reader'))
-- (derived by the generator like keys), stored as SHA-256 like real sign-ins.
INSERT INTO browser_sessions(token_hash, user_id, csrf_hash, verified_email, expires_at)
  SELECT sha256(convert_to(encode(sha256(convert_to(current_setting('omg_seed.seed') || ':session:reader', 'UTF8')), 'hex'), 'UTF8')),
         pg_temp.seed_id('user:1'), sha256(convert_to(current_setting('omg_seed.seed') || ':csrf:reader', 'UTF8')),
         'loadtest-1@loadtest.invalid', now() + interval '30 days';
INSERT INTO workspaces(id, name, kind, owner_user_id)
  SELECT pg_temp.seed_id('personal:' || i), 'Personal', 'personal', pg_temp.seed_id('user:' || i)
  FROM seed_params, generate_series(1, users) i;
INSERT INTO workspace_membership_grants(workspace_id, user_id, role, source)
  SELECT pg_temp.seed_id('personal:' || i), pg_temp.seed_id('user:' || i), 'owner', 'manual'
  FROM seed_params, generate_series(1, users) i;

-- Shared workspaces (alternating Team/Project). User i belongs to shared
-- workspace ((i-1) % shared) + 1; the first user of each is its owner. Each
-- shared workspace has one service account.
INSERT INTO workspaces(id, name, kind)
  SELECT pg_temp.seed_id('shared:' || j), 'Load shared ' || j, CASE WHEN j % 2 = 0 THEN 'team' ELSE 'project' END
  FROM seed_params, generate_series(1, shared) j;
INSERT INTO workspace_membership_grants(workspace_id, user_id, role, source)
  SELECT pg_temp.seed_id('shared:' || ((i - 1) % shared + 1)), pg_temp.seed_id('user:' || i),
         CASE WHEN i <= shared THEN 'owner' ELSE 'member' END, 'manual'
  FROM seed_params, generate_series(1, users) i WHERE shared > 0;
INSERT INTO service_accounts(id, workspace_id, name)
  SELECT pg_temp.seed_id('sa:' || j), pg_temp.seed_id('shared:' || j), 'Load service account'
  FROM seed_params, generate_series(1, shared) j;

-- Keys k = 0..keys-1: user u = k % users + 1; round r = k / users.
-- r % 4 in (0, 1): personal key; 2: member key in u's shared workspace;
-- 3: service-account key of u's shared workspace.
CREATE TEMP TABLE seed_keys AS
  SELECT k, pg_temp.seed_id('key:' || k) AS id, u, s, kind,
         CASE WHEN kind = 'personal' THEN pg_temp.seed_id('personal:' || u) ELSE pg_temp.seed_id('shared:' || s) END AS workspace_id
  FROM (SELECT k, k % users + 1 AS u,
               CASE WHEN shared > 0 THEN (k % users) % shared + 1 END AS s,
               CASE WHEN shared = 0 OR (k / users) % 4 IN (0, 1) THEN 'personal'
                    WHEN (k / users) % 4 = 2 THEN 'member' ELSE 'service' END AS kind
        FROM seed_params, generate_series(0, keys - 1) k) x;
CREATE UNIQUE INDEX ON seed_keys(k);
ANALYZE seed_keys;
INSERT INTO api_keys(id, workspace_id, issued_to_user_id, service_account_id, name, secret_hash)
  SELECT id, workspace_id,
         CASE WHEN kind <> 'service' THEN pg_temp.seed_id('user:' || u) END,
         CASE WHEN kind = 'service' THEN pg_temp.seed_id('sa:' || s) END,
         'Load key ' || k,
         sha256(convert_to('omg_' || replace(id::text, '-', '') || '.'
           || encode(sha256(convert_to(current_setting('omg_seed.seed') || ':secret:' || k, 'UTF8')), 'hex'), 'UTF8'))
  FROM seed_keys;

-- The mock upstream catalog: one local OpenAI-compatible connection (approved
-- out of band through GATEWAY_LOCAL_UPSTREAMS), one model and deployment, and
-- a v1 price of exactly 1 and 2 µUSD per input/output token.
INSERT INTO provider_connections(id, name, provider, credential_ref, endpoint, enabled)
  VALUES (pg_temp.seed_id('connection'), 'Load test mock upstream', 'openai_compatible', 'none',
          current_setting('omg_seed.endpoint'), true);
INSERT INTO models(id, public_name, display_name, supported_protocols)
  VALUES (pg_temp.seed_id('model'), current_setting('omg_seed.model'), 'Load test', ARRAY['chat_completions']);
INSERT INTO deployments(id, model_id, provider_connection_id, upstream_model, enabled)
  VALUES (pg_temp.seed_id('deployment'), pg_temp.seed_id('model'), pg_temp.seed_id('connection'), 'mock-chat', true);
INSERT INTO deployment_prices(id, deployment_id, input_microusd_per_million, output_microusd_per_million,
                              input_token_limit, output_token_limit, pricing_version)
  VALUES (pg_temp.seed_id('price'), pg_temp.seed_id('deployment'), 1000000, 2000000, 1000, 256, 1);
INSERT INTO workspace_model_grants(workspace_id, model_id, source)
  SELECT id, pg_temp.seed_id('model'), 'direct' FROM workspaces;

-- Generous but real limits, so admission evaluates the rate layer and the
-- installation and workspace-type monthly budgets exactly as production does.
DO $$ BEGIN
  IF current_setting('omg_seed.policies') = 'on' THEN
    INSERT INTO installation_policy(singleton, requests_per_minute, tokens_per_minute, concurrent_requests)
      VALUES (true, 10000000, 100000000000, 1000000)
      ON CONFLICT (singleton) DO UPDATE SET requests_per_minute = EXCLUDED.requests_per_minute,
        tokens_per_minute = EXCLUDED.tokens_per_minute, concurrent_requests = EXCLUDED.concurrent_requests;
    INSERT INTO policy_budgets(layer, period, amount_microusd) VALUES ('installation', 'month', 1000000000000000);
    INSERT INTO policy_budgets(layer, kind, period, amount_microusd)
      SELECT 'type', kind, 'month', 100000000000 FROM unnest(ARRAY['personal', 'team', 'project']) kind;
  END IF;
END $$;
