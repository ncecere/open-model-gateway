-- Provider-reported upstream model per attempt (Logs telemetry).
-- Explicit operator upgrade only (`migrate`); never applied implicitly by serve.
--
--   reported_upstream_model  the model id the provider says served the attempt
--                            (OpenAI/OpenRouter/local `model` of the body or
--                            stream frames, Anthropic `message.model`, Bedrock
--                            Converse `trace.promptRouter.invokedModelId`).
--                            Written when the attempt finishes. NULL = not
--                            reported (or not a valid bounded id); Logs then
--                            show the configured `upstream_model` snapshot,
--                            which is kept unchanged. 1-256 printable,
--                            non-space ASCII characters; never truncated.
-- Metadata only; never used for routing, authorization or accounting.
ALTER TABLE inference_executions
 ADD COLUMN reported_upstream_model text
  CHECK(reported_upstream_model IS NULL OR (char_length(reported_upstream_model) BETWEEN 1 AND 256
   AND reported_upstream_model ~ '^[!-~]+$'));
