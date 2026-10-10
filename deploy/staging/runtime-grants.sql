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
 public.workspace_type_policies,
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
 public.deployment_prices,public.workspace_type_policies,
 public.workspace_platform_policy_overrides,public.workspace_local_policies,public.key_policies,
 public.policy_budgets,
 public.inference_executions,public.governance_reservations,public.monetary_ledger,
 public.routing_policies,public.deployment_routing,public.deployment_health,public.audit_events
 TO gateway_runtime;
GRANT DELETE ON public.oidc_login_attempts,public.oidc_group_mappings,public.catalogs,
 public.catalog_models,public.workspace_type_catalogs,public.workspace_catalog_overrides,
 public.workspace_catalog_override_items,public.workspace_model_grants,
 public.key_model_selections,public.workspace_type_policies,
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
-- concurrent_jobs ("Jobs at once", 0018) is edited like the other limits. There is
-- no installation policy layer (dropped in 0026).
GRANT UPDATE(requests_per_minute,tokens_per_minute,concurrent_requests,concurrent_jobs)
 ON public.workspace_type_policies,
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
-- Installation spend rules (0026) edit their period and reference amount.
GRANT UPDATE(name,enabled,budget_layers,thresholds,spike_factor_percent,min_spend_microusd,
 window_minutes,error_rate_percent,min_requests,consecutive_failures,provider_connection_id,
 notify_workspace_admins,notify_platform_admins,notify_emails,updated_by,updated_at,deleted_at,
 spend_period,spend_amount_microusd)
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
-- Budget totals detail (0025): the unknown-cost subset, maintained by the same trigger.
GRANT UPDATE(held_unknown_microusd,unresolved_unknown) ON public.budget_totals TO gateway_runtime;
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
-- Files API (0020). stored_files gains api_purpose, written once (at insert, or
-- at commit for uploads that sent the file before its purpose; trigger), and
-- reserved_bytes, the upload's quota reservation: it only grows while the row
-- is pending (trigger). Storage quota is edited like
-- the other workspace limits (type default, override, local; no key or
-- installation column). Storage usage rows are append-only (INSERT, never
-- UPDATE/DELETE; a trigger refuses both) and the progress mark only moves
-- forward (trigger); FOR UPDATE SKIP LOCKED needs its UPDATE column.
GRANT UPDATE(reserved_bytes,api_purpose) ON public.stored_files TO gateway_runtime;
GRANT UPDATE(storage_bytes) ON public.workspace_type_policies,
 public.workspace_platform_policy_overrides,public.workspace_local_policies TO gateway_runtime;
GRANT SELECT,INSERT ON public.storage_usage_hours TO gateway_runtime;
GRANT SELECT,UPDATE(recorded_through) ON public.storage_usage_progress TO gateway_runtime;
-- Batch engine (0021). Batch price lists are a column of the append-only
-- deployment_prices (INSERT only, like every price column); reservations pin
-- their price_tier and line executions their batch_job_id at INSERT. A native
-- batch records its upstream id once and a gateway-run batch's runner holds a
-- renewable lease; result files are written once (trigger). Line rows are
-- claimed by INSERT and move running -> finished (or a counted explicit
-- retry); segments are insert-only. No DELETE or TRUNCATE.
GRANT UPDATE(upstream_id,output_file_id,error_file_id,runner_id,runner_lease_until,
 submit_started_at,in_progress_at,finalizing_at,last_progress_at) ON public.async_jobs TO gateway_runtime;
GRANT SELECT,INSERT ON public.batch_lines,public.batch_segments TO gateway_runtime;
GRANT UPDATE(state,attempts,execution_id,status_code,error_code,segment,finished_at)
 ON public.batch_lines TO gateway_runtime;
-- Batch scheduling (0022). Route settings are upserted by platform writers
-- (never deleted; the route key is fixed). The last gate evaluation of a
-- route is upserted by runners. Demand rows are scheduling state, replaced
-- and removed by runners (heartbeat). A line's route (batch_lines.deployment_id)
-- is written once at INSERT; the completion window is fixed at INSERT. The
-- stall alert reads last_waited_at, written by runners.
GRANT SELECT,INSERT ON public.deployment_batch_scheduling,public.deployment_batch_signals TO gateway_runtime;
GRANT UPDATE(max_concurrency,yield_live_threshold,metrics_url,metrics_max_waiting,
 metrics_max_running,metrics_max_kv_cache_percent,priority,window_timezone,window_days,
 window_start_minute,window_end_minute,updated_at,updated_by)
 ON public.deployment_batch_scheduling TO gateway_runtime;
GRANT UPDATE(paused_reason,checked_at,live_in_flight,metrics_checked_at,metrics_ok,
 metrics_waiting,metrics_running,metrics_kv_cache_permille,metrics_error)
 ON public.deployment_batch_signals TO gateway_runtime;
GRANT SELECT,INSERT,DELETE ON public.batch_route_waits TO gateway_runtime;
GRANT UPDATE(waiting_lines,reason,ready,last_claim_at,updated_at) ON public.batch_route_waits TO gateway_runtime;
GRANT UPDATE(last_waited_at) ON public.async_jobs TO gateway_runtime;
-- Installation logo (0023). The settings row references the current branding
-- file (a live, committed installation branding object; trigger) with its
-- dimensions; uploads/removals go through the reviewed stored_files grants.
GRANT UPDATE(branding_logo_file_id,branding_logo_updated_at,branding_logo_width,
 branding_logo_height) ON public.installation_settings TO gateway_runtime;
-- Rate counters (0024): per-minute and in-flight counters of workspaces and
-- key lineages, written only by the rate_counters_maintain() triggers inside
-- reservation/execution/async-job writes (upsert: INSERT + UPDATE of the
-- counters); keys are never re-keyed. Minute rows admission no longer reads
-- are pruned by maintenance (DELETE; a trigger refuses removing the retained
-- window). In-flight rows are never deleted. No TRUNCATE. The trigger and
-- admission call the helper functions.
GRANT SELECT,INSERT,DELETE ON public.rate_minute_counters TO gateway_runtime;
GRANT UPDATE(requests,unreserved,tokens) ON public.rate_minute_counters TO gateway_runtime;
GRANT SELECT,INSERT ON public.inflight_counters TO gateway_runtime;
GRANT UPDATE(requests,jobs) ON public.inflight_counters TO gateway_runtime;
GRANT EXECUTE ON FUNCTION public.rate_reserved_tokens(bigint,bigint,bigint,jsonb),
 public.rate_contribution(public.governance_reservations,text,uuid,boolean,text,timestamptz,integer)
 TO gateway_runtime;
-- Scoped admission (0027): admission/management take transaction advisory
-- scope locks through these helpers and lock (creating zero rows where
-- missing) the totals/counter rows they will change; the authority triggers
-- call the key/lock helpers as the invoking runtime role. No new table or
-- column privilege.
GRANT EXECUTE ON FUNCTION public.omg_scope_key(uuid),public.omg_type_key(text),
 public.omg_scope_lock_audit_order(integer),public.omg_lock_scopes(integer[],integer[],boolean),
 public.omg_admission_locks(uuid,uuid,uuid),
 public.omg_lock_scope_rows(uuid[],uuid[],timestamptz[],boolean),
 public.omg_catalog_lock_mode(),public.omg_scope_lock_exclusive(integer,integer)
 TO gateway_runtime;
-- Change notifications (0028): configuration writes bump their topic's
-- version once per transaction through the omg_config_* triggers (as the
-- invoking runtime role) and NOTIFY omg_config; every replica polls the
-- versions. Topics are seeded (no INSERT/DELETE/TRUNCATE, topic not
-- updatable) and versions only move forward (trigger).
GRANT SELECT,UPDATE(version,changed_at) ON public.config_versions TO gateway_runtime;
GRANT EXECUTE ON FUNCTION public.omg_config_bump(text) TO gateway_runtime;
-- Work leases (0029): replicas take, renew, fence and release the seeded
-- singleton-job leases through these helpers (FOR SHARE fencing needs an
-- UPDATE column). Lease names are seeded and never created, renamed or
-- removed at runtime; epochs only move forward (trigger).
GRANT SELECT,UPDATE(holder,epoch,acquired_at,expires_at,last_completed_at,last_completed_epoch)
 ON public.work_leases TO gateway_runtime;
GRANT EXECUTE ON FUNCTION public.omg_lease_acquire(text,uuid,integer),
 public.omg_lease_fence(text,uuid,bigint),public.omg_lease_complete(text,uuid,bigint),
 public.omg_lease_release(text,uuid,bigint) TO gateway_runtime;
-- History partitions (0030/0031): every grant above is on the partitioned
-- parents; partitions are reached only through them (no partition-level
-- privilege; REVOKE ALL above covers new partitions). The runtime reads the
-- registry and coverage and may only ask the SECURITY DEFINER
-- omg_ensure_partitions(ahead) for missing canonical future months (the
-- leased `partitions` job). It cannot create, attach, detach, drop or
-- truncate partitions, and the ledger's admitted_at is INSERT-only.
GRANT SELECT ON public.history_partitions TO gateway_runtime;
GRANT EXECUTE ON FUNCTION public.omg_ensure_partitions(integer),public.omg_partition_coverage(timestamptz),
 public.omg_partition_bounds(text),public.omg_month_start(timestamptz),public.omg_next_month(timestamptz)
 TO gateway_runtime;
-- Usage rollups (0032): derived, rebuildable data. The leased `rollups` job
-- replaces an hour's rows (DELETE + INSERT), records the hour and advances
-- the forward-only progress mark; the 0032 triggers append change markers
-- as the invoking runtime role and the job deletes the ones it saw. Readers
-- call omg_usage_rows. Raw history is never touched.
GRANT SELECT,INSERT,DELETE ON public.usage_rollups_hourly,public.usage_rollup_dirty TO gateway_runtime;
GRANT SELECT,INSERT,UPDATE(rolled_at,groups,lease_epoch) ON public.usage_rollup_hours TO gateway_runtime;
GRANT SELECT,UPDATE(rolled_through) ON public.usage_rollup_progress TO gateway_runtime;
GRANT EXECUTE ON FUNCTION public.omg_usage_aggregate(timestamptz,timestamptz),
 public.omg_usage_rows(timestamptz,timestamptz) TO gateway_runtime;
-- Archive (0033): operator-only (schema owner). The runtime reads the
-- records (`budget verify` adds archived contributions) and has no access to
-- schema omg_archive.
GRANT SELECT ON public.archived_partitions,public.archived_budget_contributions TO gateway_runtime;
-- History parent checks (0034): executions and reservations check their
-- workspace key, deployment, cost center, batch job and price version with
-- plain reads in BEFORE INSERT triggers (as the invoking runtime role, which
-- already reads those tables) instead of foreign keys that locked the parent
-- rows FOR KEY SHARE. Their safety rests on parents never being removed or
-- re-keyed: no DELETE/TRUNCATE on workspaces, api_keys, deployments,
-- deployment_prices, cost_centers or async_jobs and no UPDATE of their ids or
-- api_keys.workspace_id (and triggers refuse it for every role). The leased
-- history_verify job reads history and its parents and records the
-- history_orphans incident through the alert grants above. No new privilege.
-- No UPDATE/DELETE/TRUNCATE of immutable prices, ledger or audit; no removal of
-- users/workspaces/keys/history and no rewrite of immutable admission snapshots.
COMMIT;
