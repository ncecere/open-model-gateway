# AWS Bedrock

The `bedrock` adapter uses the official AWS Rust SDK's **Converse** and
**ConverseStream** operations. AWS performs endpoint resolution, SigV4 signing,
request serialization, and binary event-stream decoding. It does not use an
unsigned HTTP approximation of Bedrock.

## Deployment configuration

| Field | Required value |
| --- | --- |
| `provider` | `bedrock` |
| `upstream_model` | A Converse-compatible model ID or inference-profile ID/ARN available to the workload |
| `credential_ref` | Exactly `aws:default` |
| `region` | An explicit AWS region, for example `us-east-1` |
| `endpoint` | Absent/null; even an AWS URL or an empty string is rejected |

The adapter uses the **default AWS credential provider chain**, with the
configured deployment region. Prefer a workload role: ECS task role, EC2 instance
profile, or EKS/web-identity role. The standard chain also supports operator-managed
environment credentials and shared AWS profiles; this does not permit deployment
records to contain keys or arbitrary `env:` credential references. Credential
refresh is managed by AWS's identity provider/cache.

All deployments using `aws:default` use the gateway workload's authority. Restrict
that role to the intended models and inference profiles. It needs
`bedrock:InvokeModel` and, for streaming, `bedrock:InvokeModelWithResponseStream`.
Cross-region inference profiles can require permission for their destination
model resources as well. Model access, account quotas, and model-specific tool
support remain AWS configuration responsibilities.

The service client is built directly, **not from shared endpoint configuration**.
`AWS_ENDPOINT_URL`, `AWS_ENDPOINT_URL_BEDROCK_RUNTIME`, profile endpoint overrides,
and Bedrock bearer-token configuration are not inherited by the inference client.
There is no production endpoint injection API. Tests alone can inject a loopback
endpoint and fixed fake credentials. Standard credential discovery may contact
STS, container metadata, or instance metadata as required by the workload chain.

## Supported semantics

- Chat Completions and Messages through the gateway's canonical text/function-tool
  request model; Responses is not supported.
- Text user/assistant turns and a leading sequence of nonempty system messages.
  Developer messages, late system messages, and assistant prefills are rejected.
- Function tool definitions with object JSON schemas. `strict: true` is rejected
  rather than silently weakened; absent/false strictness is supported.
- Auto, required/any, and named-function tool choice. A named function must be
  declared. Explicit `none` tool choice is rejected because Converse has no
  equivalent that preserves the complete supplied tool configuration.
- Assistant tool calls carry parsed JSON objects, including integer values without
  conversion through floating point. Tool result **strings are preserved verbatim**
  as Bedrock text blocks, even when the string contains JSON. The canonical model
  has no distinct structured-JSON tool-result type, so reinterpreting those strings
  as JSON would change their semantics. Parallel results are grouped into a single
  user turn and must match outstanding tool-use IDs exactly once.
- Temperature must be finite and in `[0, 1]`; output token limits must be in
  `1..=i32::MAX`. Omitted settings remain unset for AWS/model defaults. Tighter
  model-specific limits are reported as provider rejections, not clamped.

Unsupported output content (images, reasoning, citations, native server tool
results, unknown variants) fails closed rather than being silently removed.
Malformed JSON tool arguments, negative token usage, inconsistent tool-use finish
reasons, and unknown stop reasons also fail closed. Model-specific unsupported
settings are sanitized provider rejections; there is no retry with altered input.

## Streaming and safety

Bedrock's binary event stream is decoded by AWS, then mapped to canonical text
and function-argument deltas. Tool indices are dense canonical indices, not AWS
content-block indices. Both implicit text-block starts and explicit empty starts
are accepted; block closure and tool argument JSON are validated.

Stop reasons map as follows:

| Bedrock | Canonical |
| --- | --- |
| `end_turn`, `stop_sequence` | `stop` |
| `max_tokens` | `length` |
| `tool_use` | `tool_calls` |
| `guardrail_intervened`, `content_filtered` | `content_filter` |

`messageStop` emits Finish, **not Done**. Metadata can subsequently emit Usage.
Done requires message start, closed content blocks, message stop, terminal
metadata, and clean transport/event-stream EOF. Truncation, CRC errors, duplicate
terminal events, late deltas, and stream exceptions produce an error, never a
successful Done. Missing usage fields remain unknown rather than fabricated zero.
Canonical events are passed to the gateway's client-protocol/SSE serializers;
Bedrock itself does not send SSE.

A small reqwest-backed AWS HTTP connector enforces an **8 MiB cumulative wire-body
limit before SDK deserialization**, including error bodies and streams. Normalized
text/tool output is limited to 4 MiB, 256 content blocks, and 128 tool calls.
Requests are also bounded. Inference requests use one SDK attempt, no reqwest
retries, no redirects, no ambient proxy, a 10-second connection limit, a 60-second
read/stream-event limit, and a 300-second transport/operation deadline. Existing
gateway deadlines can cancel sooner. No adapter producer task is spawned;
dropping the returned stream drops the AWS receiver and upstream response body.
Clients/credential caches are reused per region, with at most 64 cached clients.

Errors exposed by the adapter are gateway error enums only. No AWS response body,
credential, prompt, deployment identifier, or raw SDK error is interpolated into
them. Adapter/input state has no Debug implementation. Avoid enabling raw AWS SDK
or HTTP trace logging in production; third-party transport diagnostics are not
application-safe logging interfaces.

## Integration and verification

Constructor: `BedrockAdapter::new() -> anyhow::Result<Self>` (synchronous).
Credential discovery is lazy during execution, after request validation.
Register `Arc::new(BedrockAdapter::new()?)` with the existing provider registry.

Dependencies, in addition to existing gateway dependencies:

```toml
aws-config = "1"
aws-sdk-bedrockruntime = "1"
aws-smithy-types = { version = "1", features = ["http-body-1-x"] }
aws-smithy-runtime-api = { version = "1", features = ["client"] }
http-body = "1"
http-body-util = "0.1"
```

The Smithy dependencies bridge AWS document/HTTP types; the body dependencies
adapt the bounded stream without implementing a second AWS wire protocol.
No separate credential or signing dependency is needed.

Local tests use loopback HTTP with **explicit fake credentials**, never the host
credential chain. They exercise SDK-produced signed requests, JSON request and
response mappings, binary AWS frames fragmented across headers/payloads/CRCs,
canonical text/tool events, the shared adapter contract, malformed/truncated
streams, safe errors, one-attempt behavior, size bounds, and drop cancellation.
They do not verify a real AWS account, IAM policy, credential refresh/IMDS/STS,
model availability, or cross-region inference. Signing tests inspect the actual
AWS authorization headers; they do not independently reimplement SigV4 to verify
the signature cryptographically.

Source reference: official `awslabs/aws-sdk-rust`, commit
`2ea89feffd8b76d5b4f1d9030f38f956ec4b6957`, especially Bedrock's generated `config.rs`,
`event_stream_serde.rs`, Converse types, and `aws-config`'s default credential
provider. Initial isolated adapter checks also covered newer SDK releases. The
integrated gateway suite passes with the committed lockfile:
`aws-sdk-bedrockruntime 1.124.0`, `aws-config 1.8.13`, and
`aws-smithy-types 1.4.3`. Use `--locked` for reproducible deployment builds.
AWS dependency updates can raise compiler requirements (for example, Bedrock
1.147.0 declares Rust 1.94.1); review the toolchain and rerun the full suite when
updating the lockfile. The current verification uses stable Rust, not an MSRV CI matrix.
