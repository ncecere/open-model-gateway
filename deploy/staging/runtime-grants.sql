-- Run as gateway_migrator after EVERY enterprise migration, never as runtime.
-- Reviewed enterprise operation/column allowlist. New tables/functions fail closed.
BEGIN;
REVOKE ALL ON ALL TABLES IN SCHEMA public FROM gateway_runtime;
REVOKE ALL ON ALL SEQUENCES IN SCHEMA public FROM gateway_runtime;
REVOKE ALL ON ALL FUNCTIONS IN SCHEMA public FROM PUBLIC, gateway_runtime;
-- Table-level revocation does not remove old column privileges.
DO $$ DECLARE c record; BEGIN
 FOR c IN SELECT table_name,column_name FROM information_schema.columns WHERE table_schema='public' LOOP
  EXECUTE format('REVOKE SELECT (%1$I), INSERT (%1$I), UPDATE (%1$I), REFERENCES (%1$I) ON TABLE public.%2$I FROM gateway_runtime',c.column_name,c.table_name);
 END LOOP;
END $$;
GRANT USAGE ON SCHEMA public TO gateway_runtime;
GRANT SELECT(version,checksum,success) ON public._sqlx_migrations TO gateway_runtime;
GRANT SELECT ON public.installation,public.users,public.oidc_identities,
 public.platform_role_grants,public.effective_platform_roles,public.cost_centers,
 public.workspaces,public.oidc_group_mappings,public.workspace_membership_grants,
 public.effective_workspace_memberships,public.oidc_login_attempts,public.browser_sessions,
 public.workspace_invitations,public.service_accounts,public.api_keys,
 public.provider_connections,public.models,public.deployments,public.catalogs,
 public.catalog_models,public.workspace_type_catalogs,public.workspace_catalog_overrides,
 public.workspace_catalog_override_items,public.workspace_model_grants,
 public.key_model_restrictions,public.key_model_selections,public.deployment_prices,
 public.installation_policy,public.workspace_type_policies,
 public.workspace_platform_policy_overrides,public.workspace_local_policies,public.key_policies,
 public.policy_budgets,
 public.inference_executions,public.governance_reservations,public.monetary_ledger,
 public.routing_policies,public.deployment_routing,public.deployment_health,public.audit_events
 TO gateway_runtime;
GRANT EXECUTE ON FUNCTION public.lock_installation(),public.workspace_model_allowed(uuid,uuid),
 public.valid_model_protocols(text[]),public.valid_i64_string(jsonb,boolean),
 public.valid_cache_pricing(jsonb),public.valid_billing_usage(jsonb),
 public.valid_cost_components(jsonb),public.components_total(jsonb),
 public.valid_meter_usage(jsonb),public.valid_meter_variant(text),
 public.valid_price_lines(jsonb),public.valid_max_units(jsonb) TO gateway_runtime;
-- The singleton boundary/family/identity cannot be rewritten or removed.
GRANT UPDATE(name) ON public.installation TO gateway_runtime;
GRANT INSERT(id,email,oidc_link_allowed),
 UPDATE(email,display_name,disabled_at,cleanup_due_at,cleaned_at,disable_reason,oidc_link_allowed)
 ON public.users TO gateway_runtime;
GRANT INSERT ON public.oidc_identities,public.platform_role_grants,public.cost_centers,
 public.workspaces,public.oidc_group_mappings,public.workspace_membership_grants,
 public.oidc_login_attempts,public.browser_sessions,public.workspace_invitations,
 public.service_accounts,public.api_keys,public.provider_connections,public.models,
 public.deployments,public.catalogs,public.catalog_models,public.workspace_type_catalogs,
 public.workspace_catalog_overrides,public.workspace_catalog_override_items,
 public.workspace_model_grants,public.key_model_restrictions,public.key_model_selections,
 public.deployment_prices,public.installation_policy,public.workspace_type_policies,
 public.workspace_platform_policy_overrides,public.workspace_local_policies,public.key_policies,
 public.policy_budgets,
 public.inference_executions,public.governance_reservations,public.monetary_ledger,
 public.routing_policies,public.deployment_routing,public.deployment_health,public.audit_events
 TO gateway_runtime;
GRANT DELETE ON public.oidc_login_attempts,public.oidc_group_mappings,public.catalogs,
 public.catalog_models,public.workspace_type_catalogs,public.workspace_catalog_overrides,
 public.workspace_catalog_override_items,public.workspace_model_grants,
 public.key_model_selections,public.installation_policy,public.workspace_type_policies,
 public.workspace_platform_policy_overrides,public.workspace_local_policies,public.key_policies,
 public.policy_budgets
 TO gateway_runtime;
GRANT UPDATE(user_id) ON public.oidc_identities TO gateway_runtime;
GRANT UPDATE(revoked_at) ON public.platform_role_grants,public.workspace_membership_grants TO gateway_runtime;
-- Signature-verified email is inserted at sign-in and cleared at account cleanup.
GRANT UPDATE(revoked_at,verified_email) ON public.browser_sessions TO gateway_runtime;
GRANT UPDATE(name,cost_center_id,disabled_at,owner_user_id) ON public.workspaces TO gateway_runtime;
-- Personal provisioning uses an owner-preserving ON CONFLICT update. Applications
-- still enforce private scope; workspace kind is never mutable via runtime.
GRANT UPDATE(name,code,archived_at) ON public.cost_centers TO gateway_runtime;
GRANT UPDATE(issuer,group_value,target_kind,platform_role,workspace_id,workspace_role,enabled)
 ON public.oidc_group_mappings TO gateway_runtime;
GRANT UPDATE(revoked_at,accepted_at) ON public.workspace_invitations TO gateway_runtime;
GRANT UPDATE(disabled_at) ON public.service_accounts TO gateway_runtime;
-- disabled_at (0005) is reversible; revoked_at is set once and never cleared by the application.
GRANT UPDATE(revoked_at,disabled_at) ON public.api_keys TO gateway_runtime;
GRANT UPDATE(enabled,credential_ref) ON public.provider_connections TO gateway_runtime;
GRANT UPDATE(public_name,display_name,description,enabled,supported_protocols) ON public.models TO gateway_runtime;
GRANT UPDATE(enabled) ON public.deployments TO gateway_runtime;
GRANT UPDATE(name,description) ON public.catalogs TO gateway_runtime;
-- Row locking requires at least one UPDATE column. Grants themselves are
-- retired/reinserted through catalog operations, not silently source-rewritten.
GRANT UPDATE(model_id) ON public.workspace_model_grants TO gateway_runtime;
-- Budgets (0005) live in policy_budgets and are replaced by DELETE+INSERT per
-- scope; there is no UPDATE privilege on budget rows (amounts never rewrite in place).
GRANT UPDATE(requests_per_minute,tokens_per_minute,concurrent_requests)
 ON public.installation_policy,public.workspace_type_policies,
 public.workspace_platform_policy_overrides,public.workspace_local_policies,public.key_policies
 TO gateway_runtime;
GRANT UPDATE(state,error_code,input_tokens,output_tokens,billing_usage,elapsed_ms,completed_at,
 public_model,provider,details_redacted_at,meter_usage,output_image_variant,provider_cost_microusd)
 ON public.inference_executions TO gateway_runtime;
GRANT UPDATE(state,actual_microusd,input_tokens,output_tokens,billing_usage,cost_components,
 held_microusd,unbounded_cost,meter_usage,output_image_variant,provider_cost_microusd)
 ON public.governance_reservations TO gateway_runtime;
GRANT UPDATE(strategy,max_attempts,allow_ambiguous_failover,required_residency) ON public.routing_policies TO gateway_runtime;
GRANT UPDATE(priority,weight,residency,failure_threshold,cooldown_seconds) ON public.deployment_routing TO gateway_runtime;
GRANT UPDATE(consecutive_failures,open_until,last_observed_at) ON public.deployment_health TO gateway_runtime;
-- No UPDATE/DELETE/TRUNCATE of immutable prices, ledger or audit; no removal of
-- users/workspaces/keys/history and no rewrite of immutable admission snapshots.
COMMIT;
