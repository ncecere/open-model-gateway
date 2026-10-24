-- Same additive measures as live cost-report TOTALS, from the shared exact
-- rollup/raw fallback. No request/key/actor identifiers are projected.
WITH rows AS MATERIALIZED (
 SELECT u.*,w.kind workspace_kind,
   CASE WHEN $5 AND w.kind='personal' THEN NULL ELSE w.id END allocation_workspace_id,
   CASE WHEN $5 AND w.kind='personal' THEN 'Personal (aggregate)' ELSE w.name END workspace_name
 FROM omg_usage_rows($1,$2) u JOIN workspaces w ON w.id=u.workspace_id
 WHERE ($3::uuid IS NULL OR u.workspace_id=$3) AND ($4::uuid IS NULL OR u.cost_center_id=$4)
),
-- A single additive totals expression is used for full and dimensional totals.
groups AS (
 SELECT dimension,id,CASE WHEN dimension='cost_centers' THEN coalesce(min(name),'Unallocated') ELSE min(name) END name,
 jsonb_build_object(
  'known_cost_microusd',coalesce(sum(known_cost_microusd),0)::text,
  'held_microusd',coalesce(sum(held_microusd),0)::text,
  'attempts',coalesce(sum(attempts),0)::text,
  'unresolved_attempts',coalesce(sum(unresolved_attempts),0)::text,
  'root_requests',NULL) totals
 FROM rows CROSS JOIN LATERAL (
  VALUES ('total',NULL::text,NULL::text),
   ('workspaces',allocation_workspace_id::text,workspace_name),
   ('cost_centers',cost_center_id::text,cost_center_name),
   ('models',CASE WHEN NOT ($5 AND workspace_kind='personal') THEN public_model END,public_model),
   ('providers',CASE WHEN NOT ($5 AND workspace_kind='personal') THEN provider END,provider)
 ) d(dimension,id,name)
 WHERE dimension NOT IN ('models','providers') OR NOT ($5 AND workspace_kind='personal')
 GROUP BY dimension,id
), ranked AS (
 SELECT *,row_number() OVER(PARTITION BY dimension ORDER BY name,id NULLS FIRST) n FROM groups WHERE dimension<>'total'
), dimensions AS (
 SELECT dimension,jsonb_agg(jsonb_build_object('id',id,'name',name,'totals',totals) ORDER BY n) FILTER(WHERE n<=100) entries,
 count(*)>100 truncated FROM ranked GROUP BY dimension
)
SELECT jsonb_build_object(
 'period',jsonb_build_object('start_date',($1 AT TIME ZONE 'UTC')::date::text,'end_date',($2 AT TIME ZONE 'UTC')::date::text,'timezone','UTC','includes_partial_day',false),
 'scope',CASE WHEN $5 THEN 'platform' ELSE 'workspace' END,
 'observed_at',statement_timestamp(), 'basis','configured_rate_estimate','currency','USD',
 'totals',coalesce((SELECT totals FROM groups WHERE dimension='total'),jsonb_build_object('known_cost_microusd','0','held_microusd','0','attempts','0','unresolved_attempts','0','root_requests',NULL)),
 'breakdowns',jsonb_build_object(
  'workspaces',coalesce((SELECT entries FROM dimensions WHERE dimension='workspaces'),'[]'::jsonb),
  'cost_centers',coalesce((SELECT entries FROM dimensions WHERE dimension='cost_centers'),'[]'::jsonb),
  'models',coalesce((SELECT entries FROM dimensions WHERE dimension='models'),'[]'::jsonb),
  'providers',coalesce((SELECT entries FROM dimensions WHERE dimension='providers'),'[]'::jsonb)),
 'breakdowns_truncated',coalesce((SELECT bool_or(truncated) FROM dimensions),false),
 'truncated_dimensions',coalesce((SELECT jsonb_agg(dimension ORDER BY dimension) FROM dimensions WHERE truncated),'[]'::jsonb),
 'coverage',(SELECT jsonb_build_object('priced_attempts',NULL,'total_attempts',coalesce(sum(attempts),0)::text,
   'settled_attempts',coalesce(sum(attempts) FILTER(WHERE accounting_state='settled'),0)::text,
   'pending_attempts',coalesce(sum(attempts) FILTER(WHERE accounting_state='pending'),0)::text,
   'unknown_attempts',coalesce(sum(attempts) FILTER(WHERE accounting_state='unknown'),0)::text,
   'missing_reservation_attempts',coalesce(sum(attempts) FILTER(WHERE accounting_state IS NULL),0)::text,
   'unresolved_attempts',coalesce(sum(unresolved_attempts),0)::text) FROM rows),
 'unavailable',jsonb_build_object('root_requests','non_additive','cost_components','not_in_rollups','detailed_health','not_in_rollups','pricing_coverage','not_in_rollups','billing_usage','not_in_rollups','meter_usage','not_in_rollups','provider_reported_cost_microusd','not_in_rollups'),
 'held_is_floor',true,'historical_headroom',NULL,
 'privacy',CASE WHEN $5 THEN 'personal_aggregate_only' ELSE 'workspace_aggregate' END
)
