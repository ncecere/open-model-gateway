-- Request telemetry for Logs: per upstream attempt and per root request.
-- Explicit operator upgrade only (`migrate`); never applied implicitly by serve.
--
-- Metadata only; never prompt or response bodies.
-- Per attempt (written when the attempt finishes):
--   finish_reason           normalized: stop|length|tool_calls|content_filter
--                           (from the provider), error (the attempt failed),
--                           cancelled (client went away / lease expired) or
--                           unknown. NULL while running and for successful
--                           non-generation workloads (no finish reason exists).
--   time_to_first_token_ms  streams only: upstream dispatch to first delta.
--   generation_ms           upstream dispatch to the end of the upstream
--                           response (admission and accounting excluded).
--   reasoning_tokens        provider-reported reasoning tokens (a subset of
--                           output tokens); NULL = not reported, never zero.
-- Per attempt (snapshotted at admission, never rewritten):
--   upstream_model          the deployment's upstream model id the attempt was
--                           sent to (routes may be edited later).
-- Per root request (same value on every attempt of a root, set at admission):
--   client_session_id       optional client-supplied session id: X-Session-Id
--                           header, else OpenAI metadata.session_id / user, or
--                           Anthropic metadata.user_id. 1-128 characters, no
--                           control characters, no surrounding whitespace.
--   client_app              optional X-Title header (1-200 characters, same rules).
-- Both client values are untrusted labels for grouping/display, never used
-- for authorization or attribution, and cleared by detail retention.
ALTER TABLE inference_executions
 ADD COLUMN finish_reason text CHECK(finish_reason IN ('stop','length','tool_calls','content_filter','error','cancelled','unknown')),
 ADD COLUMN time_to_first_token_ms bigint CHECK(time_to_first_token_ms>=0),
 ADD COLUMN generation_ms bigint CHECK(generation_ms>=0),
 ADD COLUMN reasoning_tokens bigint CHECK(reasoning_tokens>=0),
 ADD COLUMN upstream_model text CHECK(upstream_model IS NULL OR char_length(upstream_model) BETWEEN 1 AND 512),
 ADD COLUMN client_session_id text CHECK(client_session_id IS NULL OR (char_length(client_session_id) BETWEEN 1 AND 128
  AND client_session_id !~ '[[:cntrl:]]' AND client_session_id = btrim(client_session_id))),
 ADD COLUMN client_app text CHECK(client_app IS NULL OR (char_length(client_app) BETWEEN 1 AND 200
  AND client_app !~ '[[:cntrl:]]' AND client_app = btrim(client_app)));
-- Sessions are grouped inside one workspace.
CREATE INDEX executions_workspace_session ON inference_executions(workspace_id,client_session_id,started_at) WHERE client_session_id IS NOT NULL;
-- Platform logs read Team/Project activity across workspaces by time.
CREATE INDEX executions_time ON inference_executions(started_at,id);
