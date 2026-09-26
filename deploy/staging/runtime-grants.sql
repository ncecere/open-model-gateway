-- Run as gateway_migrator after EVERY migration, never as gateway_runtime.
-- Explicit, reviewed operation/column allowlist; new tables fail closed.
BEGIN;
REVOKE ALL ON ALL TABLES IN SCHEMA public FROM gateway_runtime;
REVOKE ALL ON ALL SEQUENCES IN SCHEMA public FROM gateway_runtime;
REVOKE ALL ON ALL FUNCTIONS IN SCHEMA public FROM PUBLIC, gateway_runtime;
-- Table-level REVOKE does not remove old column ACLs.
DO $$ DECLARE c record; BEGIN
  FOR c IN SELECT table_name, column_name FROM information_schema.columns WHERE table_schema='public' LOOP
    EXECUTE format('REVOKE SELECT (%1$I), INSERT (%1$I), UPDATE (%1$I), REFERENCES (%1$I) ON TABLE public.%2$I FROM gateway_runtime', c.column_name, c.table_name);
  END LOOP;
END $$;
GRANT USAGE ON SCHEMA public TO gateway_runtime;
GRANT SELECT(version, checksum, success) ON public._sqlx_migrations TO gateway_runtime;
GRANT SELECT ON public.users, public.organizations, public.organization_memberships,
  public.workspaces, public.workspace_memberships, public.api_keys,
  public.provider_connections, public.models, public.deployments,
  public.workspace_model_grants, public.oidc_identities, public.login_attempts,
  public.browser_sessions, public.service_accounts, public.invitations,
  public.audit_events, public.inference_executions, public.governance_policies,
  public.deployment_prices, public.governance_reservations, public.monetary_ledger,
  public.model_routing_policies, public.deployment_routing, public.deployment_route_health,
  public.organization_model_grants, public.user_model_grants, public.platform_organization_policies
TO gateway_runtime;
-- The runtime may create ordinary OIDC users, but never set platform privileges,
-- disable identities, or execute the trusted provision-user command.
GRANT INSERT(id, email), UPDATE(oidc_link_allowed) ON public.users TO gateway_runtime;
GRANT INSERT(id, public_name, display_name, enabled), UPDATE(enabled) ON public.models TO gateway_runtime;
GRANT INSERT ON public.organizations, public.organization_memberships, public.workspaces,
  public.workspace_memberships, public.api_keys, public.provider_connections, public.deployments,
  public.workspace_model_grants, public.oidc_identities, public.login_attempts,
  public.browser_sessions, public.service_accounts, public.invitations, public.audit_events,
  public.inference_executions, public.governance_policies, public.deployment_prices,
  public.governance_reservations, public.monetary_ledger, public.model_routing_policies,
  public.deployment_routing, public.deployment_route_health, public.organization_model_grants,
  public.user_model_grants, public.platform_organization_policies TO gateway_runtime;
GRANT DELETE ON public.login_attempts, public.governance_policies,
  public.workspace_model_grants, public.organization_model_grants, public.user_model_grants TO gateway_runtime;
GRANT UPDATE(name) ON public.organizations, public.workspaces TO gateway_runtime;
GRANT UPDATE(role, disabled_at) ON public.organization_memberships, public.workspace_memberships TO gateway_runtime;
GRANT UPDATE(revoked_at, governance_key_id) ON public.api_keys TO gateway_runtime;
GRANT UPDATE(enabled, credential_ref) ON public.provider_connections TO gateway_runtime;
GRANT UPDATE(enabled) ON public.deployments TO gateway_runtime;
GRANT UPDATE(revoked_at) ON public.browser_sessions TO gateway_runtime;
GRANT UPDATE(disabled_at) ON public.service_accounts TO gateway_runtime;
GRANT UPDATE(revoked_at, accepted_at) ON public.invitations TO gateway_runtime;
GRANT UPDATE(public_name, personal_enabled) ON public.organization_model_grants TO gateway_runtime;
-- A column UPDATE privilege is required for SELECT ... FOR SHARE in admission.
GRANT UPDATE(model_id) ON public.workspace_model_grants, public.user_model_grants TO gateway_runtime;
GRANT UPDATE(state, error_code, input_tokens, output_tokens, elapsed_ms, completed_at, public_model, provider, details_redacted_at) ON public.inference_executions TO gateway_runtime;
GRANT UPDATE(state, actual_microusd, input_tokens, output_tokens, held_microusd) ON public.governance_reservations TO gateway_runtime;
GRANT UPDATE(requests_per_minute, tokens_per_minute, concurrent_requests, monthly_budget_microusd) ON public.platform_organization_policies TO gateway_runtime;
GRANT UPDATE(strategy, max_attempts, allow_ambiguous_failover, failure_threshold, cooldown_seconds, required_residency) ON public.model_routing_policies TO gateway_runtime;
GRANT UPDATE(priority, weight, residency, operator_disabled) ON public.deployment_routing TO gateway_runtime;
GRANT UPDATE(consecutive_failures, open_until, last_observed_at) ON public.deployment_route_health TO gateway_runtime;
COMMIT;
