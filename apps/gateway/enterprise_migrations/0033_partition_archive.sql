-- Operator-only archival of closed history months (scale plan P6).
-- Explicit operator upgrade only (`migrate`); never applied implicitly by serve.
--
-- `open-model-gateway archive partition <group> <YYYY-MM|legacy> --to DIR`
-- (schema owner credentials; the runtime role cannot detach or drop) exports
-- every partition of the group's month with a checksummed manifest, then in
-- one transaction records the archive below and DETACHes the partitions,
-- moving them to schema omg_archive (outside the gateway's public schema) or,
-- with --drop, dropping them. It refuses unless the month ended before the
-- configured retention, no reservation in it is pending or unknown, no
-- execution in it lacks a reservation or is still running, and every hour
-- of it has a clean usage rollup (so usage history stays answerable).
-- Nothing else is ever modified: prices, ledger, audit and totals stay.
--
-- archived_budget_contributions keeps the archived reservations' exact
-- contribution to every budget_totals bucket (per workspace, key and UTC
-- day; only settled reservations can be archived), so `budget verify`
-- still reconciles maintained totals with history after archival.
CREATE SCHEMA IF NOT EXISTS omg_archive;
REVOKE ALL ON SCHEMA omg_archive FROM PUBLIC;

CREATE TABLE archived_partitions(
 id uuid PRIMARY KEY,
 archive_group text NOT NULL CHECK(archive_group IN ('history','audit','storage')),
 month text NOT NULL CHECK(month ~ '^([0-9]{4}-(0[1-9]|1[0-2])|legacy)$'),
 lower_bound timestamptz NOT NULL,
 upper_bound timestamptz NOT NULL CHECK(upper_bound>lower_bound),
 -- [{parent, partition, rows, bytes, sha256, file}] in export order.
 partitions jsonb NOT NULL CHECK(jsonb_typeof(partitions)='array'),
 manifest_sha256 text NOT NULL CHECK(manifest_sha256 ~ '^[0-9a-f]{64}$'),
 -- History only: exact sums of the archived reservations and ledger.
 settled_microusd numeric(38,0),
 reservations bigint,
 ledger_entries bigint,
 ledger_microusd numeric(38,0),
 disposition text NOT NULL CHECK(disposition IN ('detached','dropped')),
 archived_by text NOT NULL DEFAULT current_user,
 archived_at timestamptz NOT NULL DEFAULT clock_timestamp(),
 UNIQUE(archive_group,month)
);
CREATE TABLE archived_budget_contributions(
 archive_id uuid NOT NULL REFERENCES archived_partitions(id),
 workspace_id uuid NOT NULL,
 api_key_id uuid NOT NULL,
 day_start timestamptz NOT NULL CHECK(day_start=date_trunc('day',day_start,'UTC')),
 settled_microusd numeric(38,0) NOT NULL,
 reservations bigint NOT NULL CHECK(reservations>0),
 PRIMARY KEY(archive_id,workspace_id,api_key_id,day_start)
);
CREATE TRIGGER archived_partitions_immutable BEFORE UPDATE OR DELETE ON archived_partitions
 FOR EACH ROW EXECUTE FUNCTION immutable_history();
CREATE TRIGGER archived_partitions_no_truncate BEFORE TRUNCATE ON archived_partitions
 FOR EACH STATEMENT EXECUTE FUNCTION immutable_history();
CREATE TRIGGER archived_budget_contributions_immutable BEFORE UPDATE OR DELETE ON archived_budget_contributions
 FOR EACH ROW EXECUTE FUNCTION immutable_history();
CREATE TRIGGER archived_budget_contributions_no_truncate BEFORE TRUNCATE ON archived_budget_contributions
 FOR EACH STATEMENT EXECUTE FUNCTION immutable_history();

-- Restore safety (found by the P6 backup/restore drill): pg_restore loads
-- data with an empty search_path, and CHECK validators that call other
-- validators unqualified (valid_cost_components -> valid_cost_components_base
-- -> valid_i64_string, ...) then fail on the first history row, so a
-- populated database could not be restored. Qualify those calls; the
-- function bodies are otherwise unchanged.
DO $$
DECLARE f record; def text; n text;
BEGIN
 FOR f IN SELECT p.oid FROM pg_proc p WHERE p.pronamespace='public'::regnamespace AND p.prokind='f'
   AND p.prosrc ~ '(^|[^.a-z_])(valid_i64_string|valid_cost_components_base|valid_price_lines_base|valid_model_protocols_base|components_total|valid_meter_variant)\(' LOOP
  def := pg_get_functiondef(f.oid);
  FOREACH n IN ARRAY ARRAY['valid_i64_string','valid_cost_components_base','valid_price_lines_base','valid_model_protocols_base','components_total','valid_meter_variant'] LOOP
   def := regexp_replace(def,'([^.a-z_])'||n||'\(','\1public.'||n||'(','g');
  END LOOP;
  EXECUTE def;
 END LOOP;
END $$;
