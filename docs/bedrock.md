# AWS Bedrock

The `bedrock` adapter uses the official AWS Rust SDK's **Converse** and
**ConverseStream** operations. AWS performs endpoint resolution, SigV4 signing,
request serialization, and binary event-stream decoding. It does not use an
unsigned HTTP approximation of Bedrock.

## Connection configuration

| Field | Value |
| --- | --- |
| `provider` | `bedrock` |
| `region` | An AWS region code, for example `us-east-1`. The dashboard lists common Bedrock regions and accepts any other valid code. |
| `credential_ref` | `aws:default`, `aws:profile:<name>` or `aws:role:<role-arn>` (below). Never a key. |
| `aws_external_id`, `aws_session_name` | Optional, `aws:role:` only. |
| `endpoint` | Null for the regional Bedrock Runtime endpoint, or an allowlisted VPC endpoint (below). |

Route `upstream_model` is a Converse model ID (`anthropic.claude-…`), an inference
profile ID (`us.…`, `eu.…`, `apac.…`, `global.…`) or a Bedrock ARN
(`foundation-model`, `inference-profile`, `application-inference-profile`,
`provisioned-model`, `custom-model-deployment`, `imported-model`, `prompt`). An ARN
must be in the connection's region: cross-region inference profiles are invoked from
their source region and route onward inside AWS. Management rejects other shapes and
the adapter checks them again.

### Identity modes

- **Server AWS identity** (`aws:default`): the default AWS credential provider chain
  in the connection's region. Prefer a workload role: ECS task role, EC2 instance
  profile or EKS/web-identity role.
- **Named AWS profile** (`aws:profile:<name>`): exactly that profile from the server's
  shared AWS config/credentials files, including `source_profile`, `role_arn`,
  `credential_process` and SSO profiles. Environment credentials do not take
  precedence. The name must be on `GATEWAY_AWS_PROFILE_ALLOWLIST` (comma-separated;
  default empty, so no profile is usable). Profiles are server configuration; the
  dashboard cannot create or list them.
- **Assume IAM role** (`aws:role:<role-arn>`): the server identity (default chain)
  calls STS `AssumeRole` at the connection region's STS endpoint, with the optional
  external ID and session name (default `open-model-gateway`). The STS client is built
  explicitly: one attempt, 10-second connect and 30-second operation timeouts, no
  shared endpoint configuration. The server identity needs `sts:AssumeRole` on the
  role, and the role's trust policy must allow it (and require the external ID when
  set). Which roles can be assumed is bounded by that IAM policy, not by the gateway.

Role options are folded into one canonical stored reference,
`aws:role:<arn>[;external_id=<id>][;session_name=<name>]`. Like `env:` references it
is never returned: reads expose `aws_auth` (`default`, `profile` or `role`) only. To
change a role option, restate the role (dashboard: connection › Settings › Change AWS
access). The external ID is not an AWS secret, but it is kept out of reads and logs.

Every deployment on a connection uses that connection's authority. Restrict the role
to the intended models and inference profiles. It needs `bedrock:InvokeModel` and, for
streaming, `bedrock:InvokeModelWithResponseStream`. Cross-region inference profiles
can require permission for their destination model resources as well. Model access,
account quotas and model-specific tool support remain AWS configuration
responsibilities. Credential refresh is managed by AWS's identity providers and the
client's identity cache.

### VPC endpoint (PrivateLink)

`endpoint` may replace the regional Bedrock Runtime endpoint with an interface VPC
endpoint, for example
`https://vpce-0abc123-xyz.bedrock-runtime.us-east-1.vpce.amazonaws.com`. It must be an
exact HTTPS origin (no credentials, path, query, fragment or trailing slash) that
appears on `GATEWAY_BEDROCK_ENDPOINT_ALLOWLIST` (comma-separated origins; default
empty). Requests are still signed for `bedrock` in the connection region. Redirects,
ambient proxies and implicit retries stay disabled. The override applies to Bedrock
Runtime only; STS for assumed roles uses its regional endpoint.

### Allowlists are enforced twice

Management validates identity mode, profile and endpoint allowlists, region and
route shape before saving. The adapter reads both allowlists at startup and checks
every target again before credential discovery or network I/O, so removing an entry
disables matching connections (`provider_configuration_error`) after a restart. The
database constrains reference and endpoint shapes only.

The service client is built directly, **not from shared endpoint configuration**.
`AWS_ENDPOINT_URL`, `AWS_ENDPOINT_URL_BEDROCK_RUNTIME`, profile endpoint overrides
and Bedrock bearer-token configuration are not inherited by the inference client.
Tests alone inject loopback endpoints and fixed fake credentials. Standard credential
discovery may contact STS, SSO, container metadata or instance metadata as the chosen
identity requires. One Bedrock client and credential cache is kept per region,
identity reference and endpoint (at most 64; the oldest key is evicted).

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
Clients/credential caches are reused per region, identity and endpoint, with at most 64 cached clients.

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
aws-smithy-async = "1"
http-body = "1"
http-body-util = "0.1"
```

The Smithy dependencies bridge AWS document/HTTP types (`aws-smithy-async` supplies
the time source and sleep for the explicit STS configuration); the body dependencies
adapt the bounded stream without implementing a second AWS wire protocol.
No separate credential or signing dependency is needed.

Local tests use loopback HTTP with **explicit fake credentials**, never the host
credential chain. Identity tests cover reference parsing and allowlists, a named
profile read from a temporary credentials file, and AssumeRole against a loopback STS
mock (role ARN, external ID and session name in the signed STS request) followed by a
Bedrock request signed with the assumed credentials through an endpoint override. They exercise SDK-produced signed requests, JSON request and
response mappings, binary AWS frames fragmented across headers/payloads/CRCs,
canonical text/tool events, the shared adapter contract, malformed/truncated
streams, safe errors, one-attempt behavior, size bounds, and drop cancellation.
They do not verify a real AWS account, IAM policy, real STS or PrivateLink,
credential refresh/IMDS, model availability, or cross-region inference. Signing tests inspect the actual
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
