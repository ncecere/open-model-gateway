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
-- Installation settings (0010): one seeded row; never inserted, deleted or re-keyed at runtime.
GRANT SELECT ON public.installation_settings TO gateway_runtime;
GRANT UPDATE(support_url,logo_url,human_key_max_lifetime_days,openrouter_data_collection,
 request_log_retention_days,smtp_host,smtp_port,smtp_tls,smtp_username,smtp_password_ref,
 smtp_from_address,smtp_from_name,smtp_last_test_at,smtp_last_test_ok,smtp_last_test_error,
 updated_at,updated_by) ON public.installation_settings TO gateway_runtime;
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
-- Bedrock (0008) can replace its identity reference and allowlisted VPC endpoint; the
-- profile, name and region are immutable.
GRANT UPDATE(enabled,credential_ref,endpoint) ON public.provider_connections TO gateway_runtime;
GRANT UPDATE(public_name,display_name,description,enabled,supported_protocols) ON public.models TO gateway_runtime;
GRANT UPDATE(enabled) ON public.deployments TO gateway_runtime;
GRANT UPDATE(name,description) ON public.catalogs TO gateway_runtime;
-- Row locking requires at least one UPDATE column. Grants themselves are
-- retired/reinserted through catalog operations, not silently source-rewritten.
GRANT UPDATE(model_id) ON public.workspace_model_grants TO gateway_runtime;
-- Budgets (0005) live in policy_budgets and are replaced by DELETE+INSERT per
-- scope; there is no UPDATE privilege on budget rows (amounts never rewrite in place).
-- concurrent_jobs ("Jobs at once", 0018) is edited like the other limits.
GRANT UPDATE(requests_per_minute,tokens_per_minute,concurrent_requests,concurrent_jobs)
 ON public.installation_policy,public.workspace_type_policies,
 public.workspace_platform_policy_overrides,public.workspace_local_policies,public.key_policies
 TO gateway_runtime;
-- 0009 telemetry is written at finish; client labels are cleared by detail
-- retention. upstream_model is an admission snapshot (INSERT only).
-- reported_upstream_model (0013) is the provider-reported served model, written at finish.
GRANT UPDATE(state,error_code,input_tokens,output_tokens,billing_usage,elapsed_ms,completed_at,
 public_model,provider,details_redacted_at,meter_usage,output_image_variant,provider_cost_microusd,
 finish_reason,time_to_first_token_ms,generation_ms,reasoning_tokens,client_session_id,client_app,
 reported_upstream_model)
 ON public.inference_executions TO gateway_runtime;
GRANT UPDATE(state,actual_microusd,input_tokens,output_tokens,billing_usage,cost_components,
 held_microusd,unbounded_cost,meter_usage,output_image_variant,provider_cost_microusd)
 ON public.governance_reservations TO gateway_runtime;
GRANT UPDATE(strategy,max_attempts,allow_ambiguous_failover,required_residency) ON public.routing_policies TO gateway_runtime;
GRANT UPDATE(priority,weight,residency,failure_threshold,cooldown_seconds) ON public.deployment_routing TO gateway_runtime;
GRANT UPDATE(consecutive_failures,open_until,last_observed_at) ON public.deployment_health TO gateway_runtime;
-- Alerts (0011). Rules are soft-deleted (deleted_at), never removed; scope,
-- workspace and kind are fixed at creation. Incidents are inserted by the
-- evaluator and only ever resolved once (resolved_at/resolution; a trigger
-- refuses everything else). Deliveries record counts and a category. Read
-- marks are insert-only. No DELETE/TRUNCATE on any alert table.
GRANT SELECT,INSERT ON public.alert_rules,public.alert_events,public.alert_deliveries,
 public.alert_reads TO gateway_runtime;
GRANT UPDATE(name,enabled,budget_layers,thresholds,spike_factor_percent,min_spend_microusd,
 window_minutes,error_rate_percent,min_requests,consecutive_failures,provider_connection_id,
 notify_workspace_admins,notify_platform_admins,notify_emails,updated_by,updated_at,deleted_at)
 ON public.alert_rules TO gateway_runtime;
GRANT UPDATE(resolved_at,resolution) ON public.alert_events TO gateway_runtime;
GRANT UPDATE(status,recipients,sent,failed,error,completed_at) ON public.alert_deliveries TO gateway_runtime;
-- SCIM (0014). Directory attributes are upserted per user and cleared at cleanup;
-- the user link is never re-keyed or removed. Groups and memberships are directory
-- state (DELETE allowed); group-derived grants keep their own revocation history.
-- One seeded scim_state row: last write time only.
GRANT SELECT,INSERT ON public.scim_users,public.scim_groups,public.scim_group_members
 TO gateway_runtime;
GRANT UPDATE(user_name,external_id,given_name,family_name,active,updated_at)
 ON public.scim_users TO gateway_runtime;
GRANT UPDATE(display_name,external_id,updated_at) ON public.scim_groups TO gateway_runtime;
GRANT DELETE ON public.scim_groups,public.scim_group_members TO gateway_runtime;
GRANT SELECT,UPDATE(last_write_at) ON public.scim_state TO gateway_runtime;
-- Budget totals (0015): written only by the budget_totals_maintain() triggers,
-- which run as the invoking runtime role inside reservation/execution writes
-- (upsert: INSERT + UPDATE of the counters). Bucket keys are never re-keyed and
-- rows are never deleted or truncated. Admission/report reads need SELECT.
GRANT SELECT,INSERT ON public.budget_totals TO gateway_runtime;
GRANT UPDATE(settled_microusd,held_microusd,reservations,pending,unknown,unresolved,
 unreserved_executions) ON public.budget_totals TO gateway_runtime;
-- Async jobs (0016): metadata rows only. Jobs and files are inserted once and
-- never deleted (triggers also refuse it); identity/ownership columns are not
-- updatable; state moves forward only (trigger). An admitted job keeps its
-- pending reservation until its poll deadline (lease_expires_at, extended
-- under the installation lock). The upstream-id CHECK needs EXECUTE.
GRANT SELECT,INSERT ON public.async_jobs,public.async_job_files TO gateway_runtime;
GRANT UPDATE(state,upstream_status,progress,error_code,completed_at,expires_at,cancel_requested_at,
 deleted_at,next_poll_at,last_polled_at,poll_failures,settled_at,request_total,request_completed,
 request_failed) ON public.async_jobs TO gateway_runtime;
GRANT UPDATE(job_id,claimed_by_execution_id) ON public.async_job_files TO gateway_runtime;
GRANT UPDATE(lease_expires_at) ON public.governance_reservations TO gateway_runtime;
GRANT EXECUTE ON FUNCTION public.valid_upstream_job_id(text) TO gateway_runtime;
-- Realtime (0017). A session extends its own pending hold and token reservation
-- per response window (reserved_tokens); response rows are inserted once and
-- settled once (a trigger refuses any later change), never deleted. The
-- composed validators call their 0016 bases, which need EXECUTE too.
GRANT UPDATE(reserved_tokens) ON public.governance_reservations TO gateway_runtime;
GRANT SELECT,INSERT ON public.realtime_responses TO gateway_runtime;
GRANT UPDATE(state,status,actual_microusd,floor_microusd,unbounded_cost,input_text_tokens,
 cached_text_tokens,input_audio_tokens,cached_audio_tokens,output_text_tokens,output_audio_tokens,
 cost_components,completed_at) ON public.realtime_responses TO gateway_runtime;
GRANT EXECUTE ON FUNCTION public.valid_model_protocols_base(text[]),
 public.valid_cost_components_base(jsonb),public.valid_price_lines_base(jsonb) TO gateway_runtime;
-- File store (0019): metadata only (objects are encrypted in the configured store).
-- Rows are inserted once and never deleted: removal sets deleted_at and clears the
-- filename/content type. Identity, ownership, purpose, backend and key id are fixed;
-- size/sha256/committed_at are written once and deleted rows are final (trigger).
-- Settings: per-group toggles, retention and the last storage health check.
GRANT SELECT,INSERT ON public.stored_files TO gateway_runtime;
GRANT UPDATE(size_bytes,sha256,committed_at,expires_at,deleted_at,filename,content_type,
 delete_attempts,last_delete_attempt_at,last_delete_error) ON public.stored_files TO gateway_runtime;
GRANT UPDATE(file_batch_enabled,file_batch_retention_days,file_video_enabled,
 file_video_retention_days,file_user_files_enabled,file_user_files_retention_days,
 file_export_retention_days,file_store_last_check_at,file_store_last_check_ok,
 file_store_last_check_error,file_store_last_check_target) ON public.installation_settings TO gateway_runtime;
-- No UPDATE/DELETE/TRUNCATE of immutable prices, ledger or audit; no removal of
-- users/workspaces/keys/history and no rewrite of immutable admission snapshots.
COMMIT;
