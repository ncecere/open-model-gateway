# Operations runbook

This covers health checks, metrics and alerting, backups and restore, secret rotation, Platform Admin lockout protection, upgrades, and the load-test baseline. It assumes the [staging](staging.md) role split: `gateway_bootstrap` for cluster administration, `gateway_migrator` for schema ownership, and `gateway_runtime` for the service. None of this approves a public production launch.

## Alert email outbox (#46)

For #46 apply migration 0050 explicitly and run `grants apply`/`grants verify` before starting the new workers. Stop every older delivery worker first (scale to zero, or set `GATEWAY_ALERT_INTERVAL_SECONDS=0`; unsetting it leaves the default 60 seconds, which is enabled): the database refuses an old worker's finalize while recipients are queued (SQLSTATE 55006), but an old worker sends before it writes and retries every interval, so one left running re-sends that delivery's mail. Historical terminal deliveries never replay. Pending deliveries snapshot their initially eligible recipients once, then recheck live eligibility on every claim. `alert_recipient_outbox` rows are retained (append-only), like other audit tables: cleanup only changes a row's state, never removes it. See [alerts](alerts.md#delivery) for the privacy, upgrade and retry contract.

Use incident History's Waiting/Retrying/Exhausted, waiting count and next due time to distinguish a scheduled retry from a terminal failure. There are at most five attempts per recipient within 24 hours, with 1m/5m/30m/2h backoff; every tick handles at most 25 attempts. SMTP I/O holds no database connection. Expired 90s claims recover on another worker; fenced results from old workers are ignored. A crash after acceptance can duplicate mail: neither the queue nor the relay offers network exactly-once delivery.

`gateway_alert_email_attempts_total{outcome="sent|transient_failure|permanent_failure"}` counts recorded attempt outcomes. `gateway_alert_email_deliveries{state="retrying|failed|exhausted"}` counts pending deliveries created in the last 24 hours and terminal failures/exhaustion completed in the last 24 hours (the metrics lease holder exports the installation aggregate). The completion window keeps newly expired/cleaned-up failures visible beyond the 15-minute alert window, even when they were created over 24 hours ago. Cleanup without a send does not increment the attempt counter. Failed and exhausted overlap; do not add them. The [example Prometheus rules](../deploy/monitoring/prometheus-rules.yml) include `GatewayAlertEmailPersistentFailure`. Send it through an independent operations channel, not a recursive email-failure incident inside the gateway. Missing SMTP is `not_configured`, not a retry loop. No metric labels, incident API fields or logs contain outbox fingerprints/recipient addresses. Fingerprints are pseudonymous personal data; restrict database access and backups accordingly.

## OTLP metrics (#99)

Each configured `serve` replica runs one optional OTLP/HTTP **JSON** metrics worker, independent of traces and the Prometheus listener. It renders the same scrape-derived metrics (including database-backed gauges and metrics-lease-holder omissions), then takes a bounded snapshot. No scrape listener is required; existing Prometheus scraping is unchanged. Export begins after the first interval, not during startup.

### Settings

| Variable | Default | Purpose |
|---|---|---|
| `GATEWAY_METRICS_OTLP_URL` | unset (disabled) | Full collector URL with exactly `/v1/metrics`; the path is not added. Verified HTTPS with public roots, no credentials, query or fragment. HTTP is allowed only on development loopback (`GATEWAY_ENV=development`). |
| `GATEWAY_METRICS_OTLP_HEADERS_ENV` | unset | Name of a dedicated secret variable holding `Key=Value,Key2=Value2` collector headers. At most 16 headers and 4 KiB; no control characters or transport-owned headers. Inject the value through a Secret, not a ConfigMap or public frontend setting. |
| `GATEWAY_METRICS_OTLP_INTERVAL_SECONDS` | `60` | Integer from `10` to `300`, inclusive. Overdue ticks are skipped, never replayed in catch-up bursts. |
| `GATEWAY_METRICS_OTLP_TRAFFIC` | `off` | Explicit `on` includes Traffic families, including identity-free personal-workspace aggregates. |

The other three settings require the URL; invalid settings stop startup rather than silently disabling export. The header secret must be non-reserved, absent from provider/SMTP allowlists, and not selected by another secret pointer (including trace and audit export). Metrics never reads `OTEL_*` settings or borrows another signal's headers. The [outbound address policy](#outbound-connections) applies; proxies, redirects and implicit retries are disabled. HTTP's post-DNS loopback tightening is tracked separately in #313; this slice does not claim to implement it. There are no insecure TLS, custom-CA or mTLS switches.

### Privacy and limits

The [golden inventory](../apps/gateway/src/metrics_families.golden) explicitly classifies every family as **System**, **Traffic** or **Excluded**. System health comes first, followed by opt-in Traffic, with deterministic family/series ordering within each class. Caps are 300 series per family, 2,000 logical data points per snapshot (one histogram is one point), and a 1 MiB encoded body. Traffic cannot consume these allowances ahead of System health. Disabled Traffic and Excluded families (currently build info) are skipped before label validation and do not count as drops; unclassified families are dropped.

The DB-only `gateway_alert_delivery_pending_buckets` and `gateway_alert_delivery_oldest_pending_age_seconds` gauges are **Traffic**, the same data class as `gateway_alert_email_deliveries`, despite having no labels. Only Traffic families carry `model`/`provider` labels: `model` contains platform-catalog public aliases, not arbitrary request strings; `provider` contains provider kinds. Each dynamic inventory is bounded to 200 values of at most 96 characters (overflow uses a fixed fallback). Exported label values cannot contain control characters. No workspace, user, key, request, trace or execution identities, email, session, host, IP or URL labels are exported. `path` is permitted only on scope-lock and lock-deadlock-retry families, and only as the operational enums `admission`, `settlement` or `reconciliation`, never a URL path. Prompts, responses and credentials are not exported.

The only resource attributes are `service.name`, `service.version`, `deployment.environment.name` and `service.instance.id`. The last is a random per-process UUID, never an installation or user identifier; it appears only at that resource attribute. Counters and histograms are cumulative with one process-start timestamp. If instance-ID entropy is unavailable, export stays quietly disabled for that process: no worker, fallback identity, repeated entropy draws or startup failure.

### Failures and shutdown

Collector outages never gate readiness or normal HTTP requests. One worker owns at most one snapshot/body/exchange; collection releases metrics locks and database connections before HTTP. Each exchange, including its bounded acknowledgement, has a 5-second deadline (1 second to connect). There are **no retries, queue or disk spool**: after a failed push the next interval collects a fresh snapshot.

Shutdown cancels an in-flight periodic push and attempts exactly one fresh final snapshot. Collection and delivery share **one total 3-second exporter drain budget**, concurrently with trace shutdown, not separate 3-second phases. On expiry the task is aborted and joined; dropping the runtime also aborts its work. The final snapshot is best effort, not a delivery guarantee.

Watch these System metrics (Prometheus names below; OTLP counters omit `_total`):

- `gateway_otlp_metrics_pushes_total{outcome="accepted|http_status|transport|timeout|acknowledgement"}`: completed push outcomes, including shutdown when it completes. `accepted` includes a valid partial-success acknowledgement; non-2xx, transport, deadline and malformed/oversized acknowledgement failures have fixed outcomes.
- `gateway_otlp_metrics_rejected_points_total`: collector-rejected points reported by partial success, bounded by the sent point count; they are not retried.
- `gateway_otlp_metrics_dropped_total{reason="series_limit|body_limit|label|unclassified|invalid_value"}`: points omitted locally. These and delivery outcomes appear in the **next** snapshot, never a recursive export.

The metrics exporter's own diagnostics are fixed-enum `metrics exporter state changed` warnings only on transitions into or out of an outage, not every failed push. Those warnings include no destination, header names/values, transport error text or collector response text. A DNS/egress policy refusal is additionally logged at WARN by the shared egress layer with purpose, host and resolved address, one warning per attempt (see [#321](https://github.com/ncecere/open-model-gateway/issues/321)). Monitor silence at the receiver too: a failed exporter cannot report its current failure through that same destination.

## Configuration reference

| Variable | Default | Purpose |
|---|---|---|
| `GATEWAY_INFERENCE_BASE_URL` | unset (dashboard origin + `/v1`) | Display-only SDK base URL, for example `https://api.bitop.dev/v1`, exposed read-only to signed-in users in `/api/v1/me`. Absolute HTTPS URL, no credentials/query/fragment, path ending in `/v1`; a trailing slash is removed. HTTP is allowed only on loopback with `GATEWAY_ENV=development`. Invalid/empty values stop startup. Does not configure ingress, routing or browser API calls; `GATEWAY_PUBLIC_URL` remains the dashboard origin for OIDC/CSRF. |
| `GATEWAY_METRICS_ADDR` | unset (disabled) | Separate Prometheus listener, for example `127.0.0.1:9464`. It must use a different port from `GATEWAY_LISTEN`. |
| `GATEWAY_ALERT_WEBHOOK_URL_ALLOWLIST` | unset (none approved) | Comma-separated exact canonical URLs allowed for saved alert receivers (#45 slice 1). HTTPS, or development-only loopback HTTP; no credentials/query/fragment. Save performs no DNS/network I/O. Approval never overrides egress policy. No delivery worker yet. |
| `GATEWAY_ALERT_WEBHOOK_SECRET_ENV_ALLOWLIST` | unset (none) | Dedicated comma-separated signing variable names for saved alert webhooks. `env:NAME` only, disjoint from provider/SMTP lists; reserved names fail startup. Management validates references without resolving them. Inject values directly through deployment secrets; no general `_FILE` import. |
| `GATEWAY_AUDIT_WEBHOOK_URL` | unset (no export) | HTTPS endpoint the audit log streams to. See [audit export](#audit-export). |
| `GATEWAY_AUDIT_WEBHOOK_SECRET_ENV` | unset | Name of the variable holding the webhook signing secret (at least 32 bytes). Required with the URL. |
| `GATEWAY_AUDIT_SYSLOG_URL` | unset (no export) | `syslog+tls://host[:port]` (default port 6514): export to syslog over TLS instead of the webhook. See [syslog over TLS](#syslog-over-tls). |
| `GATEWAY_AUDIT_SYSLOG_CA_FILE` | unset (public roots) | PEM file of the CA that signs the syslog receiver's certificate; when set, only it is trusted. |
| `GATEWAY_AUDIT_SYSLOG_CLIENT_CERT_FILE`, `GATEWAY_AUDIT_SYSLOG_CLIENT_KEY_FILE` | unset | PEM client certificate (chain) and private key for mutual TLS; both or neither. |
| `GATEWAY_AUDIT_OTLP_URL` | unset (no export) | An OpenTelemetry collector's full OTLP/HTTP logs endpoint, for example `https://collector:4318/v1/logs`: export as OTLP logs instead of the webhook or syslog. See [OTLP logs](#otlp-logs). |
| `GATEWAY_AUDIT_OTLP_HEADERS_ENV` | unset | Name of the variable holding headers for the collector, `Key=Value,Key2=Value2` (for example a token). Optional; only with the OTLP URL. |
| `GATEWAY_EGRESS_PRIVATE_ALLOW` | unset (none) | Private networks the gateway may connect to, as comma-separated CIDRs (`10.20.0.0/16,fd00:1::/32`). See [outbound connections](#outbound-connections). |
| `GATEWAY_SMTP_HOST_ALLOWLIST` | unset (none) | SMTP relay hosts the gateway may send to, comma-separated exact names or IP literals. The relay chosen in Admin › Settings › Email must be listed; empty means email can't be sent. See [settings](settings.md#email). |
| `GATEWAY_SECRET_ENV_ALLOWLIST` | unset | Variable names provider `env:NAME` references may read, comma-separated. Reserved names stop startup; see [credential allowlists](#credential-allowlists). |
| `GATEWAY_SMTP_SECRET_ENV_ALLOWLIST` | unset | Variable names the SMTP password reference (`env:NAME`) may read, comma-separated. Separate from `GATEWAY_SECRET_ENV_ALLOWLIST` (provider credentials). Startup fails if a name is on both lists or is one of the gateway's own credentials (`DATABASE_URL`, `GATEWAY_*`, `AWS_*`, `OPENAI_API_KEY`, `ANTHROPIC_API_KEY`). |
| `GATEWAY_DATABASE_MAX_CONNECTIONS` | `10` | Database pool size per replica (2 to 500). Size PostgreSQL `max_connections` for every replica plus migrator and backup sessions. |
| `GATEWAY_REPORTING_DATABASE_URL` | unset (primary) | Optional read-only reporting replica (or a pooler in front of one) for reports, usage and request logs only. See [read snapshots and the reporting replica](#read-snapshots-and-the-reporting-replica). Use the runtime role; set it directly (it has no `_FILE` form). An empty value is refused. |
| `GATEWAY_REPORTING_MAX_LAG_SECONDS` | `30` | Reporting replica replay lag (1 to 3600 s) beyond which reads use the primary instead. |
| `GATEWAY_ADMISSION_MODE` | `scoped` | `scoped`: admission and settlement use scoped authority and row locks (0027, [governance](governance.md#scoped-admission-lock-order)). `global`: the former protocol, serializing every admission and settlement on the installation row; an operational rollback kept for one release, needing no schema change. Any other value fails startup. |
| `GATEWAY_CONFIG_CACHE` | `on` | `on`: per-replica caches of pre-admission reads with change notifications ([governance](governance.md#caches-and-change-notifications)); admission always re-checks live. `off`: every read is live (no LISTEN connection, no version poll). Any other value fails startup. |
| `GATEWAY_LISTEN_DATABASE_URL` | `DATABASE_URL` | The one session connection per replica that LISTENs for configuration changes. LISTEN needs a session: behind a transaction-mode PgBouncer point it at PostgreSQL directly (or a session-mode pooler database), with the runtime role. Without notifications caches still follow changes through the 1 s version poll. An empty value is refused. |
| `GATEWAY_MAX_CONCURRENT_REQUESTS` | `128` (staging: 32) | Per-replica inference concurrency. Requests beyond it get 429 and are counted under `scope="gateway_capacity"`. |
| `GATEWAY_REQUEST_TIMEOUT_SECONDS` | `120` | Inference deadline. Time spent waiting for admission counts toward it. |
| `GATEWAY_FILE_STORE` | `off` | Encrypted file store: `off`, `local` or `s3`. See [file storage](file-storage.md) for the backend and encryption-key variables. |

## Credential allowlists

`GATEWAY_SECRET_ENV_ALLOWLIST`, `GATEWAY_SMTP_SECRET_ENV_ALLOWLIST` and `GATEWAY_ALERT_WEBHOOK_SECRET_ENV_ALLOWLIST` are operator-only startup settings, not management API settings. All three lists must be mutually disjoint. All refuse `DATABASE_URL`, `DATABASE_URL_FILE`, every `GATEWAY_*` or `AWS_*` name, the entire `PG*` prefix (including `PGPASSWORD`, `PGSSLKEY` and `PGPASSFILE`), `OTEL_*`, and `HTTP_PROXY`/`HTTPS_PROXY`/`ALL_PROXY`/`NO_PROXY` in either case, as well as the variables selected by these pointers (not just the pointer names):

- `GATEWAY_SCIM_TOKEN_ENV`
- `GATEWAY_S3_ACCESS_KEY_ID_ENV`, `GATEWAY_S3_SECRET_ACCESS_KEY_ENV`
- `GATEWAY_FILE_ENCRYPTION_KEYS_ENV`
- `GATEWAY_AUDIT_WEBHOOK_SECRET_ENV`, `GATEWAY_AUDIT_OTLP_HEADERS_ENV`

Other startup-owned secret names are reserved too; the two intentional exceptions are provider credentials `OPENAI_API_KEY` and `ANTHROPIC_API_KEY`, which may appear on the provider list but never the SMTP or alert-webhook lists. Use separate variables for provider, SMTP, alert-webhook and gateway secrets. Reserved entries stop startup with both failing allowlists, the offending names, and any pointer settings selecting those names; secret values are never printed. Remove it from the allowlist, use a dedicated provider/SMTP credential if needed, update the connection or SMTP reference, and restart. Never copy a gateway secret into a provider variable to bypass the refusal.

`OPENAI_API_KEY` and `ANTHROPIC_API_KEY` remain referenceable on the provider allowlist, but each produces a startup **warning**: a custom-endpoint connection using one of these references receives that credential. Prefer dedicated provider variables and review local endpoint approvals. These keys remain forbidden on the SMTP list. `PG*`, proxy and `OTEL_*` entries are refusals, not warnings. Management validation and delivery use the immutable configured allowlists; reserved-name filtering remains a second line of defence.

## Outbound connections

The gateway's own HTTP clients (OpenAI, Anthropic, OpenRouter, Bedrock requests, S3, the OpenRouter catalog, OIDC discovery, tokens and keys, the audit webhook and OTLP collector), the OpenAI realtime WebSocket, SMTP and audit syslog go through one address policy (#33):

- **Never:** unspecified, link-local (which covers cloud metadata at `169.254.169.254`), `fd00:ec2::/32`, Alibaba Cloud's `100.100.100.200`, multicast, broadcast, and reserved, documentation and benchmarking ranges. NAT64 (`64:ff9b::/96`), 6to4 and Teredo addresses are judged by the IPv4 address they carry, so IPv6-only clusters with DNS64 still reach IPv4 upstreams.
- **Loopback:** only with `GATEWAY_ENV=development`.
- **Private:** RFC 1918, `100.64.0.0/10` (carrier-grade NAT, which includes Tailscale) and IPv6 unique-local addresses, only inside `GATEWAY_EGRESS_PRIVATE_ALLOW`.
- **Public:** allowed.

Names are resolved by the gateway, refused addresses are dropped, and the connection goes only to an address that passed (so a DNS answer can't change between the check and the connection). Configured IP-literal URLs are checked at startup: `GATEWAY_S3_ENDPOINT`, `GATEWAY_BEDROCK_ENDPOINT_ALLOWLIST`, the OIDC issuer and the endpoints it publishes. A refused OIDC issuer stops startup with a message naming `GATEWAY_EGRESS_PRIVATE_ALLOW`. A refused request fails like an unreachable upstream; the log names the purpose, host and reason, and `gateway_egress_denied_total{purpose}` counts it. Approved local upstreams (`GATEWAY_LOCAL_UPSTREAMS`), including their vLLM metrics, keep their own pinned approval and need no entry here; they are refused in the same special-use ranges as above (#203), and an approval in one stops startup.

List a private network when you use one of these:

- an IdP or S3-compatible store on a private address (homelab Authentik, MinIO or RustFS);
- AWS PrivateLink or other private endpoints for Bedrock or S3 (the VPC range);
- anything reached over Tailscale or another carrier-grade NAT network: list only those addresses (for example `100.101.102.103/32`), not all of `100.64.0.0/10`.

Not covered: the AWS SDK's own credential lookups (instance metadata at `169.254.169.254`, ECS and EKS Pod Identity, STS and SSO) use its own HTTP client, because instance metadata must be reachable for credentials. Those endpoints come from the SDK and the operator's environment, never from request or tenant data. The policy is read before every command (including `healthcheck`), so a malformed `GATEWAY_EGRESS_PRIVATE_ALLOW` makes them all fail to start.

SMTP also needs its relay host on `GATEWAY_SMTP_HOST_ALLOWLIST`; a listed loopback relay (a local mail sidecar) is allowed outside development. For Kubernetes NetworkPolicy alignment see [Kubernetes › Outbound traffic](kubernetes.md#outbound-traffic).

## Audit export

With `GATEWAY_AUDIT_WEBHOOK_URL` and `GATEWAY_AUDIT_WEBHOOK_SECRET_ENV` set, the gateway streams the audit log to a SIEM (#106). [Syslog over TLS](#syslog-over-tls) (`GATEWAY_AUDIT_SYSLOG_URL`, #107) and [OTLP logs](#otlp-logs) (`GATEWAY_AUDIT_OTLP_URL`, #108) are the alternatives; there is one destination at a time. The operator controls the destination; Admin › Settings › Audit export shows the transport and status and sends a test, but can't change or stop it.

- **On and off:** a replica with a URL turns export on (and takes the `audit_export` lease). Only `off`, in `GATEWAY_AUDIT_WEBHOOK_URL`, `GATEWAY_AUDIT_SYSLOG_URL` or `GATEWAY_AUDIT_OTLP_URL`, turns it off; a replica with none of them (a rolling deploy, a debug pod) changes nothing and never delivers. Turning it on and off are themselves audit events (`audit_export.enabled`, `audit_export.disabled`), queued so the SIEM sees where a gap begins and ends. The secret's variable must not be on `GATEWAY_SECRET_ENV_ALLOWLIST` or `GATEWAY_SMTP_SECRET_ENV_ALLOWLIST`, or be a reserved name: startup refuses it, because a Platform Admin could otherwise read it through a connection or the SMTP relay and forge deliveries.

- **What:** the events the platform audit view shows (no workspace, or a team or project workspace); never personal-workspace events, prompts, responses or secrets. Export starts when it's enabled; earlier events stay in the dashboard and backups.
- **How:** each event is queued in the transaction that writes it, so none is skipped. One replica (the `audit_export` work lease) numbers queued events (`seq`, roughly oldest first: by when each event's transaction started, so a late commit can get a later number than a newer event; sort by `occurred_at` for time order) and posts them in `seq` order in batches of up to 100, removing them only after a 2xx. Failures retry with exponential backoff up to 5 minutes, forever, without dropping anything. The one exception: an event whose month was archived before it was delivered is skipped, logged and counted (`gateway_audit_export_deliveries_total{outcome="skipped"}`). Delivery is at least once: dedup on each event's `id` (a redelivered event keeps its `seq`).
- **Gap detection (#174):** `seq` starts at 1 for an installation and grows by one per exported event; a number is never reused, even after export is turned off and on (database triggers refuse rewinding the counter or renumbering, for every role). The exporter numbers queued events every two seconds, also while it is backing off. While export is on and nothing has been sent for a minute, it sends a signed **heartbeat**: `{"schema":"omg.audit.v1","installation_id":"…","sent_at":"…","heartbeat":true,"next_seq":N,"events":[]}`, meaning every number below `N` was sent or skipped. Turning export off or on, even directly in the database, writes an `audit_export.disabled` or `audit_export.enabled` event that is exported, and heartbeats stop while it is off. On the receiver, alert when a number below the latest `next_seq` never arrived (an event archived before delivery, no longer platform-visible, or removed from the queue), on `audit_export.disabled`, and when nothing at all arrives for a few minutes. Limits: events removed from the queue before they are numbered (someone with the gateway's database credentials polling the queue) leave no gap; and the counter stops at 2^53, so a counter pushed there with those credentials stops the export (silence) until an operator resets it (`ALTER TABLE audit_export_state DISABLE TRIGGER audit_export_state_sequence`, update, enable).
- **Request:** `POST` with `Content-Type: application/json`, `X-OMG-Delivery: <uuid>` and `X-OMG-Signature: t=<unix seconds>,v1=<hex>`, where `v1` is HMAC-SHA256 with the secret over `<t>.<raw body>`. Check the signature with a constant-time comparison and reject old timestamps (for example over 5 minutes) to prevent replay; each retry is signed afresh. The body:

  ```json
  {"schema":"omg.audit.v1","installation_id":"…","sent_at":"…","events":[
    {"id":"…","occurred_at":"…","action":"key.revoked","resource":{"type":"key","id":"…"},
     "actor":{"id":"…","label":"Alex Doe"},"workspace":{"id":"…","name":"Product","kind":"team"},
     "admin_token_id":null,"metadata":{"count":1},"seq":42}]}
  ```

  `actor` and `workspace` may be `null`. `actor.label` is the display name, or the email when there's none (personal data leaving the gateway), read when the event is delivered. `admin_token_id` (#37 PR2, additive) is the admin API token's own id when an admin token performed the action, `null` for a browser-session action (every event before this field existed, and still today's typical event) — never a digest or the token's plaintext. `metadata` holds only the audit log's safe fields (flags, counts, codes). A test delivery has `"test": true` and no events.
- **Destination:** HTTPS through the [outbound address policy](#outbound-connections); no redirects or proxies; 10-second timeout.
- **Watch:** `gateway_audit_export_queued` and `gateway_audit_export_oldest_seconds` (alert when the oldest event is older than, say, 15 minutes), `gateway_audit_export_deliveries_total{outcome}` (requests), and for OTLP `increase(gateway_audit_export_rejected_records_total[15m]) > 0`. The gateway's own credentials can stop the export, so also alert on the receiver on gaps and silence (above).
- **Kubernetes:** set the URL with `gateway.env` and the secret through `gateway.extraEnvFrom` from a Secret; list the variable's name in `GATEWAY_AUDIT_WEBHOOK_SECRET_ENV`. Allow the receiver in `networkPolicy.egress.httpsCidrs` if you narrowed it.

### Syslog over TLS

Set `GATEWAY_AUDIT_SYSLOG_URL=syslog+tls://host[:port]` (port 6514 by default) instead of the webhook URL (#107). Everything above applies except the request format and signature: same queue, lease, `seq` numbering, heartbeats, backoff and status.

- **Settings:** setting both URLs to destinations, or `off` in one and a destination in the other, stops startup. `GATEWAY_AUDIT_WEBHOOK_SECRET_ENV` is not used and is refused with only the syslog URL. `GATEWAY_AUDIT_SYSLOG_CA_FILE` (PEM) replaces the public roots with your CA. For mutual TLS set `GATEWAY_AUDIT_SYSLOG_CLIENT_CERT_FILE` and `GATEWAY_AUDIT_SYSLOG_CLIENT_KEY_FILE` (both or neither). The files are read once at startup; a missing or unparsable file stops startup, and the message names the setting, never the contents. Restart to pick up a renewed certificate.
- **TLS:** always; there is no plaintext TCP or UDP, also not in development. The receiver's certificate must be valid for the URL's host (SNI is the host). The address follows the [outbound address policy](#outbound-connections): loopback only with `GATEWAY_ENV=development`, private addresses only in `GATEWAY_EGRESS_PRIVATE_ALLOW`.
- **Framing:** RFC 5425 octet counting (`<length> <message>`), one RFC 5424 message per event, without structured data or BOM:

  ```text
  <109>1 2026-10-14T01:02:03.456Z <installation id> open-model-gateway - key.revoked - {"schema":"omg.audit.v1","installation_id":"…","event":{…,"seq":42}}
  ```

  Facility 13 (log audit), severity 5 (notice); the timestamp is the send time in UTC. MSGID is the event's `action`, cut to 32 characters, with anything outside printable ASCII replaced by `_`; the JSON keeps the full action. `event` is the object the webhook sends. A heartbeat has MSGID `heartbeat` and `{"schema":…,"installation_id":…,"heartbeat":true,"next_seq":N}`; a test delivery MSGID `test` and `"test":true`.
- **Delivery:** each run opens one connection, writes the batch (up to 100 messages), sends TLS close_notify and waits for the receiver to close; all within 10 seconds. Only then is the batch removed from the queue. Any failure keeps it and retries the whole batch on a new connection after the backoff, so a receiver can see a message twice (dedup on the event `id`). Errors are `connection` (resolution, the address policy or TCP), `tls` (handshake, certificate or client certificate) and `timeout`.
- **Authenticity:** syslog messages aren't signed. Without a client certificate, anyone who can reach the receiver can send events or heartbeats that look like the gateway's, and fake heartbeats would hide a stopped export. Configure the receiver to require the client certificate (mutual TLS), and trust only that certificate for this source. Encrypted private keys aren't supported; protect the key file with file permissions.
- **Message size:** RFC 5425 only requires receivers to accept 2,048 bytes; events with large metadata can be several KB. Raise the receiver's limit (rsyslog `$MaxMessageSize`, for example 64k) so messages aren't cut.
- **Guarantee:** syslog has no application-level acknowledgement, so delivery is at least once only as far as the TLS stream. A receiver that fails after reading but before storing loses those events; receivers see that as a `seq` gap. Use the webhook when you need acknowledged delivery.
- **Kubernetes:** mount the CA, certificate and key from a Secret as files, read-only, and point the `_FILE` settings (in `gateway.env`) at them; don't put PEM contents in environment variables. The chart has no volume option for this, so add the volume and mount with a patch (for example a Kustomize post-renderer). The chart's NetworkPolicy opens only 443: add a `networkPolicy.egress.extraEgress` rule for the receiver's port.

### OTLP logs

Set `GATEWAY_AUDIT_OTLP_URL` to an OpenTelemetry collector's full OTLP/HTTP logs endpoint (`https://collector:4318/v1/logs`; the path isn't added for you) instead of the webhook or syslog URL (#108). Everything in [audit export](#audit-export) applies except the request format and signature: same queue, lease, `seq` numbering, heartbeats, backoff and status.

- **A separate destination:** the audit export never reads `OTEL_*` variables (`OTEL_EXPORTER_OTLP_ENDPOINT`, `OTEL_EXPORTER_OTLP_HEADERS` and the rest). Those describe operational telemetry; the audit destination is only `GATEWAY_AUDIT_OTLP_URL`.
- **Settings:** one destination at a time: setting two URLs, or `off` in one and a destination in another, stops startup, as do `GATEWAY_AUDIT_WEBHOOK_SECRET_ENV` or the syslog `_FILE` settings with the OTLP URL. The URL follows the webhook's rules: HTTPS (plain HTTP only to loopback with `GATEWAY_ENV=development`), no credentials or fragment, the [outbound address policy](#outbound-connections), no proxy, redirects or retries, 10-second timeout (5 to connect).
- **Headers:** optionally, `GATEWAY_AUDIT_OTLP_HEADERS_ENV` names a variable holding `Key=Value,Key2=Value2`, for example `Authorization=Bearer …`. The name follows the webhook secret's rules: upper case, not on `GATEWAY_SECRET_ENV_ALLOWLIST` or `GATEWAY_SMTP_SECRET_ENV_ALLOWLIST`, not reserved, not `AWS_*` or `GATEWAY_*`. At most 16 headers; names are HTTP tokens other than `Content-Type`, `Host` and `Content-Length`; values are visible ASCII and spaces (no CR or LF; values can't contain commas). Startup checks them, and errors name the header's position, never its name or value. Values never appear in logs, errors or status. Setting the variable without the OTLP URL stops startup.
- **Encoding:** OTLP/HTTP JSON (`Content-Type: application/json`, the protobuf JSON mapping), one request per batch of up to 100 events:

  ```json
  {"resourceLogs":[{"resource":{"attributes":[
     {"key":"service.name","value":{"stringValue":"open-model-gateway"}},
     {"key":"omg.installation_id","value":{"stringValue":"…"}}]},
   "scopeLogs":[{"scope":{"name":"omg.audit","version":"omg.audit.v1"},
     "logRecords":[
       {"timeUnixNano":"1760486523456789000","observedTimeUnixNano":"1760486524000000000",
        "severityNumber":9,"severityText":"INFO",
        "body":{"stringValue":"{\"id\":\"…\",\"action\":\"key.revoked\",…,\"seq\":42}"},
        "attributes":[
          {"key":"event.name","value":{"stringValue":"key.revoked"}},
          {"key":"omg.audit.id","value":{"stringValue":"…"}},
          {"key":"omg.audit.seq","value":{"intValue":"42"}}]}]}]}]}
  ```

  One LogRecord per event. `timeUnixNano` is the event's `occurred_at` and `observedTimeUnixNano` the send time, both nanoseconds as decimal strings (64-bit integers are strings in the protobuf JSON mapping, as is `intValue`). The body is the event object the webhook sends, `seq` included, serialised as a string: parse it to read the event. A heartbeat is one record with `event.name` `omg.audit.heartbeat`, `omg.audit.next_seq`, and the body `{"schema":"omg.audit.v1","heartbeat":true,"next_seq":N}`; a test delivery one with `event.name` `omg.audit.test` and `{"schema":"omg.audit.v1","test":true}`.
- **Acknowledgement:** a 2xx removes the batch from the queue. Its body (read up to 64 KiB) can carry `partialSuccess`: when `rejectedLogRecords` is above 0, the collector refused those records for good, and OTLP says not to send them again. The gateway doesn't retry them: the batch still leaves the queue, the refused records are counted in `gateway_audit_export_rejected_records_total` (per replica; it resets on restart), and Settings › Audit export shows **Collector refusing events** with the last error `otlp_rejected` (attempts reset; it isn't "retrying") until the next batch the collector fully accepts clears it. A batch refused entirely doesn't count as a delivery: the last delivery time stays, so the backlog and silence alerts still fire. A refused heartbeat is a failure, retried with backoff. Receivers see the refused events as `seq` gaps. An empty body, `{}`, `partialSuccess` without a positive count, or a body that isn't JSON (or is over 64 KiB) means everything was accepted. A test delivery the collector refuses fails with `otlp_rejected`.
- **Errors:** any other status is `http_4xx`, `http_5xx` or `http_other`, and transport failures `connection` or `timeout`; the batch stays queued and is retried whole with the usual backoff (dedup on the event `id`, or on `omg.audit.id`).
- **Authenticity:** OTLP requests aren't signed, and there is no client certificate option. Make the collector require a token (sent in the headers), and keep the audit pipeline separate from general telemetry so other senders can't write `omg.audit` records or fake heartbeats.
- **Kubernetes:** set the URL with `gateway.env` and the headers variable through `gateway.extraEnvFrom` from a Secret; list its name in `GATEWAY_AUDIT_OTLP_HEADERS_ENV`. The chart's NetworkPolicy opens only 443: for port 4318, add a `networkPolicy.egress.extraEgress` rule.

## Traces (#98)

The gateway emits OpenTelemetry traces for one complete interactive generation path (chat completions, Responses and Messages through the shared engine), exported through a bounded, opt-in OTLP/HTTP **JSON** recorder/exporter. Automated acceptance tests run a real gateway against a local mock collector; a manual live check against Grafana Tempo 2.x (below) found a shared-workspace trace by its Logs `trace_id`: one `inference.request` root with its routing, admission, upstream and settlement spans, and only the fixed attributes. It does not instrument async jobs, batches, embeddings, images, audio, realtime sessions or other non-generation workloads, and it does not export metrics or logs (#99 tracks that separately; audit export's own, unrelated OTLP/HTTP logs destination is [above](#otlp-logs)).

- `GATEWAY_TRACES_OTLP_URL`: unset disables tracing. Set the complete `/v1/traces` URL, using verified HTTPS with public roots. No credentials, query or fragment; HTTP is allowed only on development loopback. There are no insecure TLS, custom-CA or mTLS switches.
- `GATEWAY_TRACES_SAMPLE_RATIO`: finite number in `[0,1]`, default `0.01`. Gateway root sampling and request-UUID-derived trace IDs are deterministic; a trace ID does not prove sampling or delivery.
- `GATEWAY_TRACES_OTLP_HEADERS_ENV`: optional name of a dedicated secret variable containing audit-style `Authorization=Bearer token,X-Tenant=value` headers (at most 16 headers and 4 KiB). Do not share the reference with providers, SMTP or audit export. Invalid settings fail startup without printing their values.

Use existing Helm `gateway.env`/Secret references for these settings. Private collectors need `GATEWAY_EGRESS_PRIVATE_ALLOW` covering their destination CIDRs and an explicit egress NetworkPolicy rule for the collector's port (typically TCP 4318); cloud metadata is always refused. DNS answers are policy-filtered at connection time. Redirects, ambient proxies and retries are disabled.

Recording never waits for a collector: 256 active contexts, 64 spans/request, 24 attributes and 1 KiB/span, a 2,048-span drop-on-full queue, one worker, and batches of at most 64 spans/64 KiB every second. Connect timeout is 1s; the whole exchange, including a bounded 64 KiB acknowledgement, is limited to 2s. Shutdown drains for at most 3s, then discards. Sustained export is therefore at most 64 spans per second per replica: a request produces several spans, so raising `GATEWAY_TRACES_SAMPLE_RATIO` far above the default on a busy replica fills the queue and shows up as `queue_full` drops before the collector is the limit. No disk spool or readiness dependency. Monitor `gateway_trace_dropped_spans_total{reason}`, `gateway_trace_deliveries_total{outcome}` and `gateway_trace_rejected_spans_total`; collector partial rejections are counted and never retried or logged with receiver bodies. Existing metrics are unchanged.

**Eligibility (unchanged privacy design, now live):** nothing exports without a shared-workspace admission actually committing. `SharedAdmission` is constructed in exactly one place, enforced by the compiler through module visibility, not a grep check: its constructor is private to `telemetry.rs`, callable only from that module's own `mark_shared`, which itself only accepts a token constructible solely inside a private `proof` submodule of `governance` - so only the commit branch of `governance::admit_unobserved`, once `tx.commit()` has succeeded for a live-revalidated `team`/`project` workspace, can produce one. It is never constructed for a personal workspace, never from client input, never before commit. Every request's spans are buffered the same way regardless of workspace; only a committed shared-workspace admission turns that buffer into an export. A personal request, or one that never reaches admission, exports nothing and is counted `gateway_trace_dropped_spans_total{reason="ineligible"}`.

**Spans emitted for an interactive generation request:** `inference.request` (engine entry through terminal settlement, including streamed consumption; time-to-first-token stays absent for non-streaming requests), `routing`, one `admission` per attempt (with its real sub-phase boundaries from `governance.rs`: `admission.queue`, `.connect`, `.catalog_locks`/`.authority_locks`, `.read`, `.totals_locks`, `.limits`, `.write`, `.commit`), one `upstream.attempt` per attempt (at most 3, only under the existing explicit failover policy; carries the admitted public model alias, known token counts, and first-content/generation timing), and one `settlement` per attempt (with its own real sub-phases: `settlement.queue`, `.connect`, `.locks`, `.read`, `.totals_locks`, `.write`, `.commit`). Independent background roots (`rate_prune`, `lock_wait_prune`) are recorded directly by the maintenance task, bypassing the eligibility gate entirely: they carry no workspace, user or request data of any kind, so none of the privacy design above applies to them. A `cancelled` attempt's settlement still runs (in a detached task, outside the phase scope that normally collects its sub-phase spans), so a cancelled trace legitimately has no `settlement`/`settlement.*` spans for that attempt - not a gap to chase.

**Logs correlation:** the Logs request list and request detail responses (`/api/v1/workspaces/{ws}/requests*`, `/api/v1/platform/logs/requests*`) include `trace_id`: the same deterministic hex id (`telemetry::trace_id`, derived only from the request's own id, never a client header) for a `team`/`project` row, and `null` for a personal row. An unauthorized viewer never receives the row at all, so there is no separate case to configure. The dashboard's request detail page shows a "Trace ID" field (with copy-to-clipboard) only when it is non-null.

**Finding a trace in Grafana Tempo:**

1. Open the request in Logs (Workspace or Admin) and copy its "Trace ID" field. A `null` value means the row is personal: there is nothing to look up. Shared-workspace rows have a correlation ID even when the request was not sampled.
2. In Grafana, open the Tempo data source's **Explore** view (or the "TraceQL" query type on a dashboard panel using that data source).
3. Search by trace ID directly: paste the hex value into Explore's trace ID search box (or query `{trace:id="<hex>"}` in TraceQL) and run it. A present `trace_id` only means the correlation ID was computed from this request's own ID, not a Tempo lookup of it; it does not by itself prove the trace was sampled, exported or accepted by the collector (check `gateway_trace_dropped_spans_total{reason}`, `gateway_trace_deliveries_total{outcome}` and `gateway_trace_rejected_spans_total` above - a trace that was dropped, rejected or never sampled has a `trace_id` in Logs but nothing in Tempo).
4. The resulting trace shows `inference.request` as the root span, with `routing`, `admission`(.*), `upstream.attempt` and `settlement`(.*) as its children (and `admission`/`settlement`'s own sub-phase spans nested under each), matching the span set and parentage described above.

**Live check against Tempo:** `apps/gateway/tests/telemetry_acceptance.rs`'s `shared_workspace_trace_reaches_an_external_collector` is an opt-in, `#[ignore]`d acceptance test that sends one real shared-workspace chat request through a real gateway with its exporter pointed at an actual collector, instead of the in-process mock collector the rest of that file uses. To run it against a real Tempo: port-forward Tempo's OTLP ingest (4318) and query API (3200) to loopback, then run the test with `OMG_TRACE_LIVE_COLLECTOR_URL` set to the loopback OTLP URL (development mode only allows plain HTTP on loopback; any other URL is refused with a clear error). It prints one `LIVE trace_id=<32 hex>` line; look that id up directly:

  ```sh
  kubectl port-forward -n <tempo-namespace> svc/tempo 4318:4318 3200:3200 &
  OMG_TRACE_LIVE_COLLECTOR_URL=http://127.0.0.1:4318/v1/traces \
    cargo test -p open-model-gateway --test telemetry_acceptance --features integration-tests \
    shared_workspace_trace_reaches_an_external_collector -- --ignored --nocapture
  curl http://127.0.0.1:3200/api/traces/<trace_id>
  ```

**Trace exporter scope (#98):**

- **No `OTEL_*` environment variables.** The standard OpenTelemetry SDK variables (`OTEL_EXPORTER_OTLP_ENDPOINT`, `OTEL_EXPORTER_OTLP_HEADERS`, `OTEL_SERVICE_NAME`, `OTEL_TRACES_SAMPLER`, and the rest) are never read; only the `GATEWAY_TRACES_*` settings above configure this exporter.
- **JSON only, no protobuf.** The wire format is OTLP/HTTP with a JSON body (`Content-Type: application/json`), not OTLP/HTTP protobuf or OTLP/gRPC. A collector must accept the JSON encoding on its `/v1/traces` HTTP endpoint.
- **No client trace-context propagation.** An inbound `traceparent`, `tracestate` or `baggage` header is never read: sampling and the trace ID are always the gateway's own deterministic decision from its own request ID, not a caller's. The gateway also never forwards its own trace context upstream to a provider.
- **No automatic resource/host discovery and no general tracing-event export.** Trace resource attributes are a fixed set (service name, package version, configured environment); there is no automatic hostname, process or environment-variable discovery, and the existing `tracing`/`RUST_LOG` structured log events are never turned into spans.
- **Separate OTLP signals.** This exporter sends traces only. [OTLP metrics](#otlp-metrics-99) and [audit OTLP/HTTP logs](#otlp-logs) have their own destinations and settings, independent of tracing. [Prometheus scraping](#metrics) remains available independently.

## Health endpoints

Both endpoints are on the public listener. They are not authenticated and never return error details.

- `GET /health/live` returns `200 {"status":"ok"}` without touching the database. Use it for liveness and restart decisions.
- `GET /health/ready` returns 200 only when every check passes, and 503 otherwise:

  ```json
  {"status":"ready","checks":{"database":"ok","schema":"ok","web":"ok","local_upstream_approvals":"ok"}}
  ```

  - `database`: a pooled connection answered within 2 seconds.
  - `schema`: the installation family and the full migration lineage (versions and checksums) match this binary exactly. It reports `unknown` when the database is unreachable.
  - `web`: `disabled` when `GATEWAY_WEB_DIR` is unset. Otherwise it checks that the loaded build's `index.html` still exists, which catches an unmounted volume.
  - `local_upstream_approvals`: always `ok` (#203). v0.8.0 reported `warning` while an approval in `GATEWAY_LOCAL_UPSTREAMS` was in a range v0.9.0 would refuse; that warning phase has ended. A refused approval now stops `serve` at startup (before any database connection, with a message naming the approval's host, address and category), so a replica that answers readiness has loaded only permitted approvals. The field stays so dashboards and alerts that read it keep working; `ok` carries no information beyond "this replica started". Never added to the ready/not-ready decision. To check an installation before upgrading, see [the upgrade note](releases/unreleased.md#local-upstream-approvals-in-special-use-ranges-are-now-refused-203).

  Use readiness for load-balancer membership and the container `HEALTHCHECK`. The service never migrates by itself, so a release whose schema does not match stays unready until you run `migrate`.

### `open-model-gateway healthcheck`

The runtime image has no shell or curl, so the binary probes itself:

```sh
open-model-gateway healthcheck [--url http://127.0.0.1:8080/health/ready] [--timeout 3s] [--allow-non-loopback]
```

It exits 0 for a `2xx` response and 1 otherwise (non-2xx, redirect, refused connection, timeout). Without `--url` it probes `/health/ready` on the port of `GATEWAY_LISTEN` over loopback (`127.0.0.1`, or `::1` for `[::]`). It accepts plain `http://` only, never reads proxy variables, never follows redirects or retries, refuses non-loopback destinations unless `--allow-non-loopback` is given, and reads no configuration, `.env` file or secret. The image's `HEALTHCHECK` is `["/usr/local/bin/open-model-gateway", "healthcheck", "--timeout", "3s"]`. Compose can override it in exec form:

```yaml
healthcheck:
  test: ["CMD", "/usr/local/bin/open-model-gateway", "healthcheck"]
```

In Kubernetes, prefer native HTTP probes (`httpGet` on `/health/live` for liveness and `/health/ready` for readiness, port 8080). If a probe must be an exec probe, use the same command: `command: ["/usr/local/bin/open-model-gateway", "healthcheck"]`. Never use `sh -c` or `curl`; neither exists in the image. The Helm chart (`deploy/helm/open-model-gateway/`, scale plan P7) wires this and the rest of this runbook's PgBouncer, migration and CNPG guidance together; see [Kubernetes](kubernetes.md).

## Container image

The runtime stage is `gcr.io/distroless/cc-debian12` pinned by digest: glibc, libgcc_s, CA certificates and the gateway binary, with the built SPA in `/app/web`. There is no shell, package manager, curl, perl, Node or Cargo. It runs as UID/GID `10001:10001` with `ENTRYPOINT ["/usr/local/bin/open-model-gateway"]` and `CMD ["serve"]`, works with a read-only root filesystem and needs no writable path (staging still mounts a small `/tmp` tmpfs). The binary is PID 1 and handles `SIGTERM` itself; Compose's `init: true` is optional.

What the former `container-entrypoint.sh` did is now done by the binary at startup, for every subcommand except `healthcheck`, before it reads `.env` files or configuration:

- Sets the process umask to `077`.
- Imports `DATABASE_URL`, `GATEWAY_OIDC_CLIENT_SECRET`, `OPENAI_API_KEY` and `ANTHROPIC_API_KEY` from a file named by `<NAME>_FILE` (Compose or Kubernetes secret mounts; symlinks are followed). Only these four names; other `*_FILE` variables and custom provider references (`GATEWAY_SECRET_ENV_ALLOWLIST`) are never interpreted, so inject those directly. The file must be a readable regular file; one terminal LF is removed. The `_FILE` variable is removed after import.
- Refuses to start (exit 1) if both a variable and its `_FILE` companion are set, even to an empty value, or if any of the four values is empty, longer than 4096 bytes, or contains CR, LF or NUL. Messages name the variable and the reason, never the value or file path.
- Runs exactly the given subcommand: `serve` never migrates or bootstraps. Run one-off commands with the same image, for example `docker run --rm … IMAGE migrate`.

To debug a running container without a shell, use `docker exec CONTAINER /usr/local/bin/open-model-gateway healthcheck`, `docker logs`, `docker inspect` (user, health log), or attach a debug container that shares its namespaces (`docker debug`, or `kubectl debug --target`).

## Metrics

Metrics are served only when `GATEWAY_METRICS_ADDR` is set. They use a dedicated listener that answers `GET /metrics` with OpenMetrics text (`application/openmetrics-text; version=1.0.0`) and returns 404 for every other path. Metrics are never on the public port and never behind the SPA fallback. The endpoint has no authentication, so bind it to loopback or a private scrape network. In the staging Compose file the variable is passed through but empty by default, and Caddy never routes to it. Prometheus 2.x and 3.x negotiate OpenMetrics automatically. The exposition also parses as classic text: `promtool check metrics` reports only the expected HELP lint for OpenMetrics `_total` naming.

Labels are deliberately low-cardinality:

- Route templates, never raw paths. Unrouted and SPA requests share `route="unmatched"`.
- Methods come from a fixed set.
- Provider kinds and configured public model names are read back from the admitted execution row. Client-supplied model strings are never used as labels. These labels are bounded to 200 distinct values each; anything beyond that becomes `other`.
- Safe error codes and limit scopes.

There are no workspace, key, user, request IDs, or prompt and response data in any label.

| Metric | Type | Labels |
|---|---|---|
| `gateway_build_info` | gauge (1) | `version` |
| `gateway_http_requests_total` | counter | `method`, `route`, `status` |
| `gateway_http_request_duration_seconds` | histogram | `method`, `route` (time to response headers; for streams this is time to first byte) |
| `gateway_inference_attempts_total` | counter | `provider`, `model`, `outcome` (`succeeded`, `failed`, `cancelled`) |
| `gateway_inference_attempt_errors_total` | counter | `provider`, `code` |
| `gateway_upstream_duration_seconds` | histogram | `provider`, `model` (dispatch to end of upstream response) |
| `gateway_upstream_time_to_first_token_seconds` | histogram | `provider`, `model` (streams) |
| `gateway_inference_tokens_total` | counter | `provider`, `model`, `direction` (provider-reported only; unknown usage is not counted as zero) |
| `gateway_settlements_total` | counter | `outcome`: `settled` (exact cost), `unknown` (hold retained), `held` (finalization failed or timed out after 3 s; the reservation stays pending until reconciliation) |
| `gateway_admission_seconds` | histogram | `phase`, `outcome`. Wall-clock time of each phase of a durable interactive admission transaction: `queue` (in-process wait for an admission slot), `connect` (pool acquire and `BEGIN`), `locks` (scoped: the shared catalog and authority locks, one statement; global: the installation row lock wait), `price` (global mode only: shared catalog lock and latest price, before the installation lock), `read` (scoped: latest price, live authorization and the deployment check with the admission clock; global: authorization and deployment check), `lock_rows` (scoped: the workspace's and lineage's totals and counter rows, the wait for other admissions of the same workspace), `limits` (policies, budgets with their totals and the rate counters, one statement), `write` (execution, reservation and hold in one statement with trigger fan-out), `commit`, and `total`. Scoped admissions of one workspace serialize from `lock_rows` to `commit`; in the global mode everything from `locks` to `commit` is serialized installation-wide. Settlement phases: `queue`, `connect`, `locks` (its reservation row), `read`, `lock_rows` (its totals rows), `write`, `commit`. `outcome` is `admitted`, `denied` (a limit or budget denial), `rejected` (other refusals such as an unavailable model) or `error` (database failure). Phases after an early return are not observed. Buckets run from 100 µs to about 52 s. Batch-line and realtime-window admissions are not included. |
| `gateway_settlement_seconds` | histogram | `phase` (`queue`, `connect`, `locks`, `read`, `lock_rows`, `write`, `commit`, `total`), `outcome` (`settled`, `unknown`, `replay`, `conflict`, `error`). Terminal settlement of an interactive attempt (`finish`); a replay stops after `locks`. |
| `gateway_lock_deadlock_retries_total` | counter | `path` (`admission`, `settlement`, `reconciliation`). Governance transactions re-run after PostgreSQL aborted them as a deadlock victim (SQLSTATE 40P01; at most two re-runs, never of upstream work). The canonical lock order keeps it at 0; a rise means a session changed authority scopes out of order. |
| `gateway_scope_lock_wait_seconds` | histogram | `lock` (`authority`: the shared workspace-type/workspace/user/key advisory locks; `rows`: the workspace's and key lineage's totals and counter rows), `path` (`admission`: new consumption, including batch lines and realtime growth; `settlement`: settlement and lease reconciliation). Scoped mode only. Time to acquire scoped admission locks: `rows` waits are other transactions of the **same** workspace or key, so a rising `rows`/`admission` tail is one hot scope approaching its [ceiling](#per-scope-admission-ceiling). No workspace or key labels; the in-app `admission_ceiling` alert names the scope. Buckets 100 µs to about 52 s. |
| `gateway_scope_lock_waiting` | gauge | `path`. Governance transactions of this replica waiting for scoped locks right now. |
| `gateway_admission_denials_total` | counter | `code`, `scope` (`workspace`, `api_key`, `policy` for rate/concurrency limits, `gateway_capacity`; `installation` no longer occurs since installation-wide limits were removed in 0026). `code="job_limit_exceeded"` counts video/batch jobs refused by a "Jobs at once" limit, with the scope that refused them. |
| `gateway_reservations_held` | gauge | `state` (`pending`, `unknown`). Refreshed at scrape time, at most every 15 seconds, with a 2-second query timeout. Summed from the workspace `lifetime` rows of `budget_totals` (one row per workspace, exact), not by counting history. Since 0026 there is no installation-scope row. Installation-wide: since 0029 only the replica holding the `metrics` work lease exports it, so `sum` and `max` across replicas agree (briefly both during a handover). |
| `gateway_cache_lookups_total` | counter | `cache` (`keys`, `candidates`, `routes`, `health`, `prices`), `result` (`hit`, `miss`, `bypass`: versions unconfirmed or caching off). Hit rate = hit / (hit + miss + bypass). |
| `gateway_config_changes_total` | counter | `topic` (`access`, `catalog`, `keys`, `policy`, `settings`): configuration versions this replica observed advancing. |
| `gateway_config_poll_failures_total` | counter | none. Failed 1 s version polls; after 3 s without a success every cache is bypassed. |
| `gateway_config_listener_up` | gauge | none (1 while the LISTEN connection is up). |
| `gateway_work_leases_held` | gauge | `lease` (`alerts`, `compaction`, `file_sweep`, `lifecycle`, `maintenance`, `metrics`): 1 on the replica holding it. Exactly one replica should report 1 per lease. |
| `gateway_work_lease_terms_total` | counter | `lease`: terms this replica started (acquisitions and takeovers). |
| `gateway_background_runs_total` | counter | `job`, `result` (`ok`, `failed`, `fenced`: the term ended during the run and nothing was committed). Singleton jobs on this replica. |
| `gateway_alert_evaluations_total` | counter | `result` (`ok`, `skipped` when another replica holds the lock, `failed`) |
| `gateway_alert_rule_failures_total` | counter | none |
| `gateway_db_pool_connections` | gauge | `state` (`idle`, `in_use`) |
| `gateway_db_pool_max_connections` | gauge | none |
| `gateway_metrics_collection_errors_total` | counter | `collector` (`reservations`; `scope_lock_waits`: a failed or skipped flush of this replica's per-scope lock-wait counts, or counts dropped beyond 20,000 unflushed scope-minutes) |
| `gateway_file_store_operations_total` | counter | `backend` (`local`, `s3`), `op` (`put`, `get`, `read`, `head`, `delete`, `health`), `outcome` (`ok` or a safe error code such as `integrity`, `denied`, `unavailable`). `op="read"` counts failures while streaming an object. |
| `gateway_file_store_bytes_total` | counter | `backend`, `op` (`put`; `get` counts objects read to the end). Plaintext bytes. |

Gauges and counters are per replica. Aggregate them with `sum`. The reservation gauge counts the whole installation and is exported by one replica (the `metrics` lease holder); `max` across replicas is the safest aggregation.

### Alerting and dashboards

Example monitoring files are in [`deploy/monitoring/`](../deploy/monitoring/):

- `prometheus.example.yml`: scrape configuration for `job="open-model-gateway"`.
- `prometheus-rules.yml`: example alerts, validated with `promtool check rules`. They cover metrics down, 5xx rate, inference p95 latency, held settlements, growing unknown reservations, a stuck pending floor, budget denials, capacity saturation, upstream failure rate, time to first token, pool saturation, alert-evaluator failures, and collector errors. The thresholds are starting points.
- `grafana-dashboard.json`: import it and select a Prometheus data source. It has `instance` and `provider` variables and overview, HTTP, inference, accounting, and internals rows.

Also probe `/health/ready` from outside, for example with blackbox-exporter, because metrics alone cannot tell you the public listener is reachable.

How to respond:

- **Held settlements:** these are database or finalization failures. Holds are retained and never refunded as zero. The maintenance loop moves expired leases to `unknown`. Fix database health first, then review unknown usage in the dashboard and resolve it deliberately.
- **Unknown reservations:** they keep budget holds and can block budgeted admission (`unresolved_usage`). Resolve them; never delete reservations.
- **Pool saturation or rising latency:** read the [load test](#load-test-baseline) section before raising pool sizes. Compare `histogram_quantile(0.99, sum by (le, phase) (rate(gateway_admission_seconds_bucket{outcome="admitted"}[5m])))` across phases: a large `lock_rows` share means one workspace is hot (its admissions serialize on its totals rows; split the traffic over workspaces; `gateway_scope_lock_wait_seconds{lock="rows"}` shows the same wait and an [`admission_ceiling`](alerts.md#rule-kinds) rule names the scope); a large `locks` share means a management change of a scope (or, in the global mode, the installation lock) is held; a large `queue` share means the per-replica admission slots (half the pool) are busy; a large `limits` share means per-minute rate accounting or budget reads are slow (see [pg_stat_statements](#finding-slow-queries-and-lock-waits)).

## Backups and restore

`scripts/backup.py` wraps `pg_dump` and `pg_restore` with checksums and lineage checks:

```sh
# Backup: custom format, without owner or ACLs. Writes <dump> and <dump>.manifest.json (both 0600).
python3 scripts/backup.py backup --url-file .local/staging/secrets/migrator_database_url --out-dir /secure/backups

# Verify the checksum, size and archive readability (pg_restore --list).
python3 scripts/backup.py verify /secure/backups/gateway-...dump.manifest.json

# Restore only into an explicitly named EMPTY database (or a new one with --create),
# checking the lineage against the binary of the release that will serve it.
python3 scripts/backup.py restore /secure/backups/gateway-...dump.manifest.json \
  --url-env MIGRATOR_ADMIN_URL --target-db gateway_restore_20261009 --create \
  --gateway-binary /path/to/open-model-gateway
```

What the script guarantees:

- **Connection handling:** URLs come from `--url-env` (default `DATABASE_URL`) or `--url-file`. They are passed through `PG*` environment variables and are never printed or placed on a command line.
- **Containers:** `--pg-container NAME` runs the PostgreSQL 17 client tools inside that container with `docker exec`. The URL is then resolved inside the container, for example `127.0.0.1:5432`.
- **Extensions:** the gateway schema uses none, so backups dump only the built-in `plpgsql` (`pg_dump --extension=plpgsql`). Extensions a cluster's `template1` adds to every database, such as pgvector, are left out: the migrator restoring on a clean host usually may not create them.
- **Manifest contents:** the SHA-256 and size of the dump, plus `schema_family`, `latest_version` and the full migration list (version and checksum), the server version, and the source database name. `file_encryption_key_ids` lists the distinct file-store key IDs that live `stored_files` rows use (IDs only, never key material; empty when the file store is unused). Encrypted backups also record `encryption` (see below). The manifest is plaintext JSON and holds no secrets.
- **Backup refusals:** a backup refuses empty or dirty lineages, or a lineage that changed while the dump ran. Partial files are deleted.
- **Restore checks:** restore verifies the checksum first. It then requires the backup lineage to equal `open-model-gateway schema-version`, which prints the binary's embedded migrations as JSON without database access. The weaker `--expect-version N` is available when no binary is at hand.
- **Restore safety:** target names must be lowercase identifiers. System and demo databases are refused. The target must contain no relations, schemas, functions, types or extensions. Restore uses `pg_restore --single-transaction --exit-on-error`. If it fails, it drops only a database it created in that same run. It never cleans, drops or overwrites existing data. Afterwards it re-reads the restored lineage.
- **File-store keys:** if the manifest lists file key IDs, restore refuses unless `--file-keys-env NAME` names a variable holding the key list (`kid:base64key[,…]`, the format of the variable named by `GATEWAY_FILE_ENCRYPTION_KEYS_ENV`) that contains every listed ID. Only the IDs are parsed; values are never printed. Pass `--skip-file-key-check` only when stored files will not be recovered.
- **Sensitive contents:** backups contain identities and accounting data. Without `--encrypt-to` the dump is plaintext on disk; use encrypted backups for anything that leaves the host. Keep secrets, configuration, and Caddy certificate volumes in a separate encrypted recovery plan; the dump does not contain them.

### Encrypted backups

Encryption uses [age](https://age-encryption.org) (`age` and `age-keygen` on the host that runs the script):

```sh
# Once: create the identity on a trusted machine, store it in the secrets manager
# and keep only the public key (age1...) where backups are taken.
age-keygen -o backup-identity.txt   # prints "Public key: age1..."

# Backup: pg_dump is streamed through age into <dump>.age; the plaintext dump is never written.
python3 scripts/backup.py backup --url-file .local/staging/secrets/migrator_database_url \
  --out-dir /secure/backups --encrypt-to age1...    # repeatable; or --encrypt-to-file recipients.txt

# Verify the ciphertext; with --identity also decrypt (in memory) and check the plaintext and pg_restore --list.
python3 scripts/backup.py verify /secure/backups/gateway-...dump.age.manifest.json --identity /run/secrets/backup-identity.txt

# Restore: two decrypt passes, no plaintext on disk.
python3 scripts/backup.py restore /secure/backups/gateway-...dump.age.manifest.json \
  --url-env MIGRATOR_ADMIN_URL --target-db gateway_restore_20261013 --create \
  --gateway-binary /path/to/open-model-gateway \
  --identity /run/secrets/backup-identity.txt --file-keys-env GATEWAY_FILE_KEYS
```

- **Backup:** while the dump streams into `age`, the script computes the plaintext SHA-256 and size and feeds the same stream to `pg_restore --list` to prove it is a readable archive. The manifest records the plaintext `sha256`/`bytes`, `file` (the `.age` name) and `encryption`: `tool`, the recipients' public keys, `ciphertext_sha256` and `ciphertext_bytes`.
- **Verify:** always checks the ciphertext checksum and size. Only `--identity` checks the plaintext.
- **Restore:** requires `--identity`. Pass 1 decrypts to check the plaintext SHA-256 and `pg_restore --list`. Pass 2 decrypts again straight into `pg_restore --single-transaction`. It holds back the last block until the plaintext checksum matches again, so an archive that changes between passes never completes a restore.
- **Multiple recipients:** use one for the routine identity and one for an offline break-glass identity. Any one of them can decrypt.

### Off-host copies (S3-compatible storage)

`upload`, `list`, `download` and `prune` talk to S3-compatible object storage (AWS S3, RustFS, MinIO) with path-style requests signed with AWS Signature Version 4. They use only the Python standard library. `--endpoint` is the storage origin (default `https://s3.<region>.amazonaws.com`); plain `http://` is accepted only for loopback hosts. `--region` defaults to `us-east-1`. Credentials come only from `AWS_ACCESS_KEY_ID` and `AWS_SECRET_ACCESS_KEY` (and optionally `AWS_SESSION_TOKEN`). They are never accepted as arguments or printed. Requests do not follow redirects, use proxies or retry.

```sh
export AWS_ACCESS_KEY_ID=... AWS_SECRET_ACCESS_KEY=...   # from the secrets manager; a key limited to this bucket
S3="--endpoint https://rustfs.example.internal --region us-east-1"

python3 scripts/backup.py upload /secure/backups/gateway-...dump.age.manifest.json --to s3://omg-backups/prod $S3
python3 scripts/backup.py list --from s3://omg-backups/prod $S3          # newest first
python3 scripts/backup.py download gateway-...dump.age --from s3://omg-backups/prod --out-dir /restore $S3
python3 scripts/backup.py prune --from s3://omg-backups/prod --keep 30 $S3        # prints the plan
python3 scripts/backup.py prune --from s3://omg-backups/prod --keep 30 --yes $S3  # deletes
```

- **Upload** refuses unencrypted backups unless `--allow-plaintext` is passed. It re-checks the local checksum, refuses to overwrite a generation whose manifest already exists, and uploads the archive first and the manifest last. After each PUT it checks the size with a HEAD. A manifest therefore only exists once its archive is complete; an interrupted upload leaves at most an archive without a manifest, which `list` and `prune` ignore. Archives over 5 GiB are refused: the script uses single-request PUTs and has no multipart upload.
- **Download** writes the manifest and archive with mode 0600, refuses to overwrite existing files, and verifies the archive checksum against the manifest. It deletes what it wrote if the check fails. Then run `verify --identity` and `restore` as above.
- **Prune** keeps the newest N generations (`--keep` ≥ 1, so never the newest one) and only prints the plan without `--yes`. It deletes each generation's manifest first, then its archive. It only deletes pairs it recognizes: a `gateway-…dump[.age].manifest.json` that parses and names its archive, with the archive present. Other objects, nested prefixes and archives without a manifest are left alone. Use bucket versioning or object lock as well if you need protection against a compromised credential deleting backups.

### Key custody

Keep three things apart, in a secrets manager such as OpenBao, with separate access policies:

1. **The age identity** (`AGE-SECRET-KEY-…`). Only the public key lives where backups are taken. Store the identity in the secrets manager, plus an offline break-glass copy, and never in the bucket or next to the backups.
2. **The file-store key list** (`GATEWAY_FILE_ENCRYPTION_KEYS_ENV`), every generation still referenced by stored files. The manifest's `file_encryption_key_ids` says which IDs a restore needs. Keep retired keys until no backup you retain references them. See [file store encryption keys](#file-store-encryption-keys).
3. **The object-store credentials** for the backup bucket. They can read ciphertext and, unless restricted, delete it. They cannot decrypt anything.

Anyone holding the backup bucket's credentials and the age identity can read every database backup. Stored file contents are not in these backups: each object in the file store carries its own wrapped data key, so anyone holding the file-store key list and read access to the file store's bucket (or its copies) can read stored files; the age identity plays no part there. Keep each of these pairs apart: the age identity away from the backup bucket's credentials, and the file-store keys away from the file store's storage credentials.

### What this covers

- **Covered:** a logical dump of the gateway database at the moment it was taken, encrypted, checksummed, copied off host and restorable into a new empty database with lineage and file-key checks.
- **Not covered:** point-in-time recovery (see below), stored file contents (back up the file store's bucket or directory separately; it holds ciphertext only), secrets and configuration, and certificates. Retention policy, bucket replication and object lock are configured in the storage system, not by this script.

`python3 scripts/staging.py backup|restore-check` remains the staging-stack shortcut. Its restore-check always uses a newly named disposable database.

### Point-in-time recovery

A logical dump only gives you a recovery point at the time it was taken. For a smaller RPO, use your PostgreSQL platform's PITR: a managed service's PITR, or base backups plus continuous WAL archiving with pgBackRest, WAL-G or `archive_command`, retained for at least the RPO window and stored encrypted off host. To recover:

1. Restore the base backup to a **new** cluster.
2. Set `recovery_target_time` to just before the incident.
3. Promote the recovered cluster.
4. Run the restore drill checks below against it before cutting over.

Never recover over the live cluster. Never run down migrations. Never delete pending or unknown reservations to make the numbers reconcile.

For CNPG base backups and WAL archiving, failover behaviour and connection sizing with several replicas, see [HA PostgreSQL operations](postgresql-ha.md).

### Restore drill (quarterly and after schema changes)

1. Take a fresh backup and run `verify`.
2. Restore with `--create` into `gateway_restore_<date>` on a non-production cluster, using `--gateway-binary` from the release you intend to run.

   If the migrator may not create databases (common with operator-managed clusters), a database administrator creates the target instead, then you restore without `--create`:

   ```sql
   CREATE DATABASE gateway_restore_20261014 OWNER <migrator> TEMPLATE template0;  -- template0: no template extensions
   REVOKE ALL ON DATABASE gateway_restore_20261014 FROM PUBLIC;                  -- what --create does
   GRANT CONNECT ON DATABASE gateway_restore_20261014 TO gateway_runtime;
   ```

   Without the `REVOKE`, `grants verify` fails with "runtime can create objects" (PUBLIC's default `TEMP`). `--create` itself copies `template0`, for the same reason.

   Demo database names are refused as targets. To cut a restored copy over to the name the gateway expects, stop every gateway replica, then a database administrator, connected to another database such as `postgres`, drops the old database and renames the restored one into its place (`DROP DATABASE …`, `ALTER DATABASE gateway_restore_… RENAME TO …`). Both need ownership, `RENAME` also needs `CREATEDB`, and neither runs while sessions are open on those databases.
3. Apply the runtime grants as the migrator with `open-model-gateway grants apply`, then run `open-model-gateway grants verify` with a role that can `SET ROLE gateway_runtime` (it runs `deploy/staging/verify-privileges.sql`, rollback-only).
4. Start the matching release against the restored database. `/health/ready` must report `schema: ok`.
5. Compare the restored ledger and execution counts (printed by `restore`) with the source. Spot-check costs, and confirm that pending and unknown reservations survived. Run `open-model-gateway budget verify` against it.
6. Record the elapsed time (your measured RTO) and the backup age (RPO), then drop the drill database.

**What a restore brings back, and what it loses.** A restore returns the database to the moment of the backup. Before serving it:

1. **Revocations and suspensions made after the backup are undone.** API keys, membership and platform role grants, browser sessions and invitations revoked since the backup, and users, keys, service accounts and workspaces disabled since then, are active again in the restored copy.
   - If the old database can still be read (corruption, a bad change, a lost replica, not a total loss), copy its revocations onto the restored one before starting the gateway:

     ```sh
     python3 scripts/backup.py reapply-revocations --source-url-file old.url --url-file restored.url        # dry run: counts per table
     python3 scripts/backup.py reapply-revocations --source-url-file old.url --url-file restored.url --yes  # one transaction
     ```

     It copies revocations (`revoked_at`) and suspensions (`disabled_at`, with a user's reason and cleanup date) only onto rows still active in the restored copy, never clears anything, applies in one transaction and refuses two databases of different installations. Something re-enabled after the backup stays disabled until you re-enable it. Run it as the migrator on both.
   - If the old database is gone, so is its audit log. Take what changed since the backup from your off-host audit export and repeat it in the dashboard: `key.revoked`, `key.disabled`, `role.revoked`, `workspace.manual_membership_revoked`, `invitation.revoked`, `scim.user.deactivated`, `service_account.updated` and `workspace.updated`. Suspensions and reactivations are `user.suspended` and `user.reactivated` (from this release; before it they were `user.updated` without detail, so check older suspensions by hand). Automatic suspensions that follow a removed role or group mapping are not separate events: replay the `role.revoked` or mapping change instead. Without an export, revoke every key created before the incident that you can't vouch for, and have users sign in again.
2. **Sessions.** If the incident involved a compromise, revoke every session before serving, so everyone signs in again: `UPDATE browser_sessions SET revoked_at = now() WHERE revoked_at IS NULL;` as the migrator.
3. **Spend recorded after the backup is lost.** Budgets and limits count from the restored totals, so a workspace may spend again what it already spent. Compare provider invoices for the gap, and tighten affected budgets until the period rolls over.

**Clean-host drill (2026-10-14 UTC, #105)** on the homelab cluster, with two disposable `dev-omg` environments (PostgreSQL 18, release `dev-bc760061ffa5`, schema version 37):

- **Scenario:** the source installation is lost (its gateway stopped and never used again). The newest encrypted off-host backup in RustFS is the recovery point. A fresh environment on another database cluster takes over.
- **Acknowledged data:** a team created through the API before the backup (must survive) and one created 34 s after it (the expected loss).
- **RPO:** the backup snapshot was taken at 02:46:11 UTC and the failure declared at 02:47:00, so 48 s of writes were at risk. Exactly the one acknowledged write in that window was lost: one workspace, its membership grant and its audit event. All 71 tables matched the pre-backup snapshot row for row. With logical dumps the RPO is the backup interval: a daily backup can lose up to a day.
- **RTO: 92 s** from failure to a gateway serving the restored database:
  - fresh environment (cluster, migrate, bootstrap, gateway): 73 s;
  - target created and download verified: 2.4 s;
  - restore with two decrypt passes and lineage checks: 7.2 s;
  - rename into place, `grants apply`, `grants verify`, `budget verify` and `history verify`: 2.7 s;
  - gateway restart and readiness: 6.4 s.

  A fresh SSO sign-in as the platform admin then worked, and Admin › Teams showed the pre-backup team but not the lost one.
- **Found and fixed:**
  - The first attempt failed because the backup carried `pgvector` from the cluster's `template1`, which the migrator may not create. Backups now leave template extensions out.
  - An administrator-created target needs the database `REVOKE` above; `grants verify` caught it.
- **Limits:**
  - The demo has no file store, so file-key completeness was trivially satisfied (no key IDs).
  - Logical dump only: no point-in-time recovery.
  - The demo database is small (a 0.5 MB compressed dump) and restores in seconds. A large installation's restore time grows with its size; time your own.

Local evidence (2026-10-09) on a freshly migrated v14 database on the disposable 54339 cluster, with tools in the container:

- Backup took 0.3 s (151 kB) and `verify` passed.
- `restore --create --gateway-binary` succeeded, and the restored copy passed the binary's enterprise preflight.
- A second restore into the now non-empty target was refused, as was `--expect-version 99`.
- All drill databases were dropped.

Partitioned drill (2026-10-10, P6): a 0033 database with 6,000 attempts across the legacy and two month partitions (12,000 ledger rows, 600 unknown) backed up, verified and restored with `--create --gateway-binary` in 2 s; every partition's ledger count and sum, the unknown reservations and `partitions status` matched, and `budget verify` reported no mismatch on both. The drill found that restores of any populated database failed before 0033 (CHECK validators calling other validators unqualified under `pg_restore`'s empty `search_path`); 0033 fixes it.

`tests/backup.test.py` covers the refusal paths with mocks. Encryption tests run the real `age`, which they require. Object-store tests use an in-process S3 server that verifies every request's SigV4 signature. The SigV4 code is also checked against AWS's documented examples. Its opt-in integration cases, including an encrypted round trip, run in CI against the service database.

## Secret rotation

General rule: add the new credential, deploy, verify, then revoke the old one. Never paste secrets into tickets or logs.

- **Database runtime/migrator passwords:**
  1. As `gateway_bootstrap`, run `ALTER ROLE gateway_runtime PASSWORD '<new>'`.
  2. Update `secrets/runtime_password` and `runtime_database_url`.
  3. Restart the gateway. Existing pooled sessions keep working until they reconnect, so restart promptly.
  4. Rotate the migrator the same way, between releases.

  Do not rerun `staging.py init`.
- **Provider API keys:** keys are referenced as `env:NAME` and must be listed in `GATEWAY_SECRET_ENV_ALLOWLIST`. Write the new key to the secret file, then restart. To rotate with no gap, create the new key at the provider first and revoke the old one after `gateway_inference_attempt_errors_total{code="upstream_rejected"}` stays flat. Prefer workload identity for Bedrock.
- **OIDC client secret:** register the new secret at the IdP, replace `secrets/oidc_client_secret`, restart, test a sign-in, then revoke the old secret. See [identity](identity.md) for IdP signing-key handling.
- **SMTP password:** update the referenced secret, restart, then send a test from Settings.
- **S3 static keys for the file store:** create the new key pair, update the variables named by `GATEWAY_S3_ACCESS_KEY_ID_ENV`/`GATEWAY_S3_SECRET_ACCESS_KEY_ENV`, restart, use **Test storage**, then revoke the old pair. Prefer `aws:role:` or workload identity on AWS.
- **File store encryption keys:** see [below](#file-store-encryption-keys).
- **Inference API keys** belong to users and workspaces. Rotate them through the dashboard; rotation keeps policy and spend lineage. Revoked keys never reactivate.

## File store encryption keys

The [file store](file-storage.md) encrypts every object under the first key in the variable named by `GATEWAY_FILE_ENCRYPTION_KEYS_ENV` (`kid:base64key[,kid:base64key…]`).

**Backup.** Keep the key list in your secret manager and in the same encrypted recovery plan as the database secrets, **separately from object and database backups**. A database backup holds `stored_files` metadata (owners, sizes, hashes, key IDs) but no contents. An object backup (bucket replication or versioning, or a copy of `GATEWAY_FILE_STORE_DIR`) holds ciphertext only. Without the keys, objects are unrecoverable. After a restore, run `open-model-gateway files verify`. It reports missing objects, size mismatches and key IDs that are no longer configured.

**Rotation.**

1. Generate a key (`openssl rand -base64 32`) with a new ID and put it **first**: `k2027a:<new>,k2026a:<old>`. Restart every replica. New objects use `k2027a`, and old ones still decrypt.
2. Move the existing files: `open-model-gateway files rekey --from k2026a --to k2027a` (add `--dry-run` first to see the count). It re-wraps each object's data key under the new key; the contents are copied back unchanged, never decrypted or re-encrypted, so it costs one read and one write of each object (on S3, egress and request charges for the whole bucket). Each object is replaced atomically (a rename on local disk; a single PUT, or a multipart upload completed at the end, on S3), one file at a time; a copy that doesn't come out the same length is abandoned and the old object kept. The command is safe to interrupt and rerun, and safe while the gateway serves. It prints a JSON report and exits nonzero while any file still uses the old key: `failed` (rerun to retry), `skipped_deleting` (a delete was in progress; rerun after a few minutes), `other_backend` (files recorded under another store; move or delete them first).
3. Run `files verify`: `by_key_id` counts every live file per key, and `retired_key_ids_in_use` lists decrypt-only keys still needed. Files on legal hold are re-wrapped like any other.
4. Remove the old key only when it is no longer in `retired_key_ids_in_use`, and once no backup you keep references it (a restored database or file-store backup needs the key its files were written under). Removing it earlier makes those files fail with `key_unavailable` (and `verify` lists the ID under `unknown_key_ids`).

The same procedure works for local disk and S3-compatible stores. On a versioned bucket the old object versions keep the old wrapped key until they expire. Each file is rewritten under its row lock (with no idle timeout, so a slow copy keeps it): deleting that file (through the Files API, a batch cleanup or a logo replacement) waits until its rewrite finishes, and the sweeper skips it until then. Unfinished uploads aren't re-wrapped; they are removed within a day.

**Compromise.** If a master key leaks, add a new active key at once. A re-wrap does not help against someone who also copied the ciphertext: the data keys inside stay the same. Treat the files under the old key as exposed to anyone who also has the ciphertext, and expire or delete them (shorten the group's retention and run `files sweep --once`) before removing the key. `files rekey` protects only against a future copy of the ciphertext being read with the leaked key.

## Platform Admin lockout protection

Neither SCIM nor the management API can remove platform access from the last active Platform Admin (a live Admin grant of any provenance on an account that is not suspended or cleaned up).

- **SCIM:** deactivation, `DELETE`, and Group changes that would drop the last effective Admin grant are refused whole with `409` (`scimType: "mutability"`). See [SCIM](scim.md#the-last-platform-admin-is-protected).
- **Manual:** Admin › Users refuses to revoke the last Admin grant or suspend the last Admin, and group mappings that hold it cannot be changed or deleted (`409 Cannot remove the last platform administrator`).
- Both checks run under the installation lock, so concurrent changes cannot both pass. (Scoped admission never takes that lock; it serializes management changes only.)

When SCIM is refused, the gateway writes the audit event `scim.last_admin_protected` and opens the built-in installation alert "SCIM tried to remove the last Platform Admin" (Notifications, Admin › Settings › Alerts › History, and email to Platform Admins when a relay is set up). It means the identity provider wants to remove the only Admin. Grant Admin to a second person (a manual grant, or a mapped Admin group), then let the provider retry or repeat the change. The alert clears at the next evaluation once a second active Platform Admin exists. Keeping two Admins avoids the situation entirely. A sign-in claim that drops the last group-provenance Admin grant is not covered by this check.

## Read snapshots and the reporting replica

**Management route preflight (#195).** After session/CSRF verification, the declared access level is checked in a short read-only snapshot on the **primary**, before handler body extraction. Platform authority takes one SELECT through the live role view; workspace authority takes one SELECT joining the existing live role/workspace/membership views, sharing the same authorization decision code as locked handlers. Key routes add one scoped key SELECT. `WorkspacePlatformWrite` is deliberately admin-first: one platform-role SELECT refuses non-admins before any workspace lookup; admins then get a workspace-only metadata SELECT, without reading the platform role again. An explicit `rollback()` ends the preflight snapshot on success and refusal before entering the handler: it never holds a second pool connection alongside the handler's transaction. Handlers retain their own checks and were not reordered.

**Measured net request cost (#195).** Local development binaries, before `origin/main` at `59c759e` (including #199) and after the route preflight/shared snapshot-query change; PostgreSQL 17, a fresh database and gateway per version, restricted runtime role, default pool 10, `GATEWAY_CONFIG_CACHE=off`. Each endpoint had 50 warm-ups followed by 500 sequential authenticated HTTP GETs over a persistent loopback connection; every response was `200`. No provider was called. Times include the retained handler, not just preflight:

| Endpoint | Before median / p95 (ms) | After median / p95 (ms) |
| --- | --- | --- |
| `/api/v1/workspaces/{ws}/keys` | 5.817 / 8.583 | 6.769 / 9.814 |
| `/api/v1/platform/providers` | 3.637 / 4.470 | 6.018 / 11.559 |

The median increases were 0.952 ms and 2.381 ms respectively. Fewer authority SELECTs do not eliminate the extra snapshot's transaction setup/rollback round trips; workspace handlers also benefit from the shared combined SELECT. These small, shared-host development samples are not production capacity or latency guarantees. Budget primary capacity for the preflight even when report data uses a replica.

Reports (`cost-report`, `cost-summary`, `costs`, `usage-export`), usage (`usage/overview`, `usage/explore`, `usage`, `executions`), request logs (`requests`, `generations`, `sessions`, `logs/metrics`, workspace and platform), `/me`, `/me/summary`, `/me/keys`, `/me/models`, `/v1/models` and the directory lists (platform users, user detail and workspaces; workspace members, keys and service accounts) take **no installation lock and no catalog lock** (scale plan P1, R06). Each runs in one `REPEATABLE READ READ ONLY` transaction with a 10 s statement timeout (plus the handlers' 10 s deadline):

- **Authorization is inside the snapshot.** The live checks (user active, platform role, membership or personal ownership, workspace not disabled; for `/v1/models` the full key revalidation) run in the same snapshot as the data, without row locks. Privacy rules are unchanged: personal workspaces stay owner-private, platform readers see personal totals only, members see their own activity and workspace admins the whole workspace.
- **Bounded staleness.** A read is consistent as of its first statement. A revocation (membership, platform role, user disable, key revoke, workspace disable) that commits while a read is running does not change that read, so it may still return what the caller could see when it started; the window is bounded by the read's duration (at most about 10 s). The next request takes a new snapshot and is denied.
- **Inference is unaffected.** Admission is unchanged: it revalidates the key, user, grants and workspace live under its own locks, so a revocation blocks new upstream work immediately. `/v1/models` is only a listing.
- **No interference.** Reads neither wait for admission, settlement, catalog writes or management changes, nor make them wait. Before P1 a 10 s report held the installation lock and stalled every admission behind it.
- **Cancellation-safe transaction start.** Every transaction begins through `db::begin`, which finishes `BEGIN` (and the snapshot's `SET TRANSACTION`) in its own task. With sqlx 0.8, a begin that is dropped mid-flight, for example by a client disconnect or page navigation, returns the pooled connection inside a transaction sqlx does not track. Later plain statements run inside that transaction, and the next snapshot fails with `25001` (an intermittent 503). `clippy.toml` rejects direct `Pool::begin`.
- **Diagnosable 503s.** When a management handler answers 503 "Management storage unavailable", it logs a `management storage error` warning with the request id, the SQLSTATE and the constraint, table, column and routine names. The PostgreSQL message is logged only for classes whose messages never contain values. Row data (DETAIL) and bound parameters are never logged.

**Reporting replica.** Set `GATEWAY_REPORTING_DATABASE_URL` to an asynchronous hot standby (or a pooler in front of one) to move report, usage and log *data* off the primary. `/me`, `/v1/models`, admission, settlement, reconciliation and every write always use the primary.

- The caller is authorized first on the primary (lock-free snapshot), so authorization is as fresh as without a replica; the data then comes from a snapshot on the replica.
- The pool connects lazily: an unreachable replica never blocks startup or `/health/ready`. A read falls back to the primary snapshot it authorized in when the replica cannot be reached within 1 s, errors, or replays more than `GATEWAY_REPORTING_MAX_LAG_SECONDS` behind (lag is 0 when caught up; otherwise the age of the last replayed commit). Each fallback logs a warning with the reason, never the URL.
- Use the runtime role (roles and grants replicate from the primary). Replica settings: `hot_standby_feedback=off` so reports never hold back vacuum on the primary, and `max_standby_streaming_delay` around 30 s; a report cancelled by recovery conflict returns 503 and can be retried.
- The replica pool has the same size as `GATEWAY_DATABASE_MAX_CONNECTIONS`; count it in the standby's `max_connections`.

## Upgrade and migration procedure

1. Read the release notes for migrations and any grant changes. Build or pull the image by digest.
2. Take a backup and `verify` it. Run a restore drill for releases that include migrations.
3. For incompatible migrations, drain traffic: stop ingress, then the gateway. Mixed-schema replicas are not supported.
4. Run `open-model-gateway migrate`, then `open-model-gateway grants apply` (reapplies `deploy/staging/runtime-grants.sql`, embedded in the binary), both as the migrator. `staging.py migrate` does both.
5. Start the new release. Wait for `/health/ready` to report `schema: ok`, then check metrics, sign-in, and one bounded inference request.
6. To roll back, redeploy the previous image digest only if no migration ran. Otherwise restore the pre-upgrade backup into a new database and cut over. There are no down migrations.

`0018_job_limits.sql` adds the "Jobs at once" limit to every policy layer and sets the workspace-type defaults to 2 active video/batch jobs per workspace. After upgrading, review Admin › Settings › Defaults & limits if workspaces routinely run more jobs at once. It also adds the SCIM last-admin alert kind. Reapply `runtime-grants.sql` (step 4).

`0026_remove_installation_limits.sql` (breaking) removes installation-wide limits: the installation budget (every period) and installation requests/tokens per minute, requests at once and jobs at once. Before upgrading, move any installation limit you still need to the workspace-type defaults, platform workspace overrides or keys. The migration records the removed values in the audit log (`policy.installation_removed`), converts installation budget alert rules to non-blocking installation spend rules, drops `installation_policy` and deletes the derived installation rows of `budget_totals`. It briefly freezes history writes (like 0015/0025), so drain traffic (step 3). Then reapply `runtime-grants.sql` (step 4) and run `open-model-gateway budget verify` (expect `mismatch_count: 0`). Clients of `/api/v1/platform/installation/policy` now get `410 reason:"installation_limits_removed"`. See [governance](governance.md#no-installation-wide-limits).

`0030_partition_history.sql` and `0031_partition_audit.sql` (scale plan P6) partition request history by UTC month without rewriting it (except the ledger, below); `0032_usage_rollups.sql` adds hourly usage rollups and `0033_partition_archive.sql` operator archival. Drain traffic (step 3) like every migration. For large installations, run `open-model-gateway partitions prepare` with the migrator **before** the drain: it builds the new keys with `CREATE INDEX CONCURRENTLY` while the old release keeps serving, so the drained window only validates and swaps. After step 4 run `open-model-gateway budget verify`, `open-model-gateway partitions status` and `open-model-gateway rollups run --once` (catch-up of existing history; `serve` continues it every minute). Details and measurements: [History partitions](#history-partitions-rollups-and-archival-p6).

`0034_history_parent_checks.sql` (decision gate D3) replaces the foreign keys from executions and reservations to workspaces, keys, deployments, price versions, cost centers and batch jobs with insert-time checks that do not lock the parent, and makes those parents impossible to delete, truncate or re-key (triggers, for every role). It changes no history row and takes only brief metadata locks (35 ms on 2 M and 43 ms on 10 M seeded attempts). Drain traffic (step 3) like every migration, reapply `runtime-grants.sql` (no new privilege), then run `open-model-gateway history verify` once (it should report `finding_count: 0`). Operator scripts that deleted an unreferenced workspace, key, deployment or cost center now fail: disable, revoke or archive instead. Details: [MultiXacts and history parent checks](#multixacts-and-history-parent-checks).

## History partitions, rollups and archival (P6)

**Partitions.** `inference_executions` (by `started_at`), `governance_reservations` and `monetary_ledger` (by `admitted_at`), `audit_events` (by `created_at`) and `storage_usage_hours` (by `hour_start`) are range-partitioned by UTC month. Rows from before the upgrade stay in each table's `<table>_p_legacy` partition (everything before the month after the newest row at upgrade time); later months get `<table>_pYYYY_MM`. Partitions are reached only through their parents: the runtime has no privilege on a partition, every partition has a TRUNCATE guard, row triggers (immutability, totals, counters, rollup markers) are defined on the parents, and an UPDATE that would move a row to another month is refused. `history_partitions` lists the partitioned tables; readiness accepts their partitions (`pg_class.relispartition`) and still rejects every other unknown relation.

How the upgrade converts large tables (each numbered step of 0030/0031 is its own transaction and is idempotent, so a failed or interrupted `migrate` resumes where it stopped):

1. Build the new keys (which must include the month) as unique indexes on the existing tables: `SHARE` lock, so writes wait and reads continue (or nothing, after `partitions prepare`).
2. Add `CHECK (key < cutover) NOT VALID`, then `VALIDATE` it under `SHARE UPDATE EXCLUSIVE` (reads and writes continue).
3. A short transaction renames the table to `<table>_p_legacy`, creates the partitioned parent with the same columns, checks, indexes, foreign keys and triggers, and attaches the old table: the validated check proves the bound, and the prebuilt indexes and validated foreign keys are adopted, so nothing is scanned or rebuilt under the exclusive lock.
4. The reservation → execution key now includes the time (`admitted_at = started_at`, as admission always wrote); it is added `NOT VALID` and validated before the swap. The migration refuses, without changing anything, if any reservation was admitted at a different time than its execution started.
5. The ledger gains `admitted_at` (copied from its reservation), so it is copied once into its partitioned replacement while the old ledger is `SHARE` locked (reads continue), compared by count, amount sum and a per-row hash, and swapped at the end of that transaction. This is the one rewrite.

Measured on the disposable 54339 cluster (PostgreSQL 17, laptop, parallel workers off) with 2,000,000 seeded attempts over 120 days (2 M executions, 1.96 M reservations, 3.9 M ledger rows, 1,000 audit events): `migrate` 0030–0033 took 27.7 s (0030 27.66 s, of which most is the ledger copy and the key builds; 0031 16 ms; 0032 6 ms; 0033 3 ms). The longest continuously observed `ACCESS EXCLUSIVE` lock on a history table was 0.68 s, `SHARE` (writes wait) 21.4 s; a reader of recent reservations polled every 20 ms throughout never waited more than 5 ms. Counts, sums and a hash of every ledger row were identical before and after, and `budget verify` took 9.0 s.

**Future months.** There is no default partition: a write outside every partition fails (admission fails closed, nothing is dispatched). `serve` keeps `GATEWAY_PARTITIONS_AHEAD_MONTHS` (default 3, 2..12) future months under the `partitions` work lease, hourly, through `omg_ensure_partitions(ahead)`: a `SECURITY DEFINER` function that can only create canonical month partitions of registered tables (`CREATE TABLE … LIKE` then `ATTACH`, so the parent is never `ACCESS EXCLUSIVE` locked; `lock_timeout` 5 s, retried next hour). While any table covers fewer than 2 future months the built-in installation incident **History tables are running out of monthly partitions** (`partitions_missing`) is open (critical below 1), Platform Admins get email, and `gateway_history_partition_months_ahead{table}` shows the coverage. Operators can run the same with the migrator:

```sh
open-model-gateway partitions status          # read-only; exits 1 below 2 future months
open-model-gateway partitions ensure --ahead 3
```

**Hourly usage rollups.** `usage_rollups_hourly` keeps exact integer sums per UTC hour, workspace, key, route, model alias, provider, cost-center snapshot, execution state and accounting state. Unknown is kept apart (`accounting_state` `unknown`, or none for attempts without a reservation) and never summed as zero. The `rollups` lease holder recomputes an hour once it ended before every running gateway transaction began (and at least an hour ago), at most 168 hours per minute; later writes to rows of an ended hour (settlement of a request that crossed the hour, a resolved unknown cost, operator corrections) append a change marker in the same transaction, and readers serve an hour from its rollup only when no marker is visible in their snapshot, so rollup-backed answers equal the raw scan exactly. Usage Explore over 7 days or more (spend, tokens, cache hit rate; not request counts, which are not additive) and the spend-spike alert's 7-day baseline read rollups; cost reports, overview tiles and logs stay on raw history (they need per-attempt prices, meters and distinct request counts). `GATEWAY_USAGE_ROLLUPS=off` makes every read raw. After an upgrade, `open-model-gateway rollups run --once --budget-seconds N` backfills existing history faster than the per-minute job.

**Archival (operator only).** Default retention is unchanged: nothing is deleted. To move a closed month out of the hot tables, with the migrator:

```sh
open-model-gateway archive partition history 2024-01 --to /secure/archive   # executions, reservations, ledger
open-model-gateway archive partition audit 2019-01 --to /secure/archive --retention-months 84
open-model-gateway archive verify /secure/archive/history-2024-01-<id>       # checksums, no database
```

- **Refusals (nothing changes):** the month (or `legacy`) ends within `GATEWAY_HISTORY_RETENTION_MONTHS` (default 25) of the current month; for `history`, any reservation of the month is pending or unknown (holds must settle first), any execution of it is running or has no reservation (unknown cost), or any hour with history lacks a clean usage rollup; the month is already archived. The runtime role cannot archive: detaching and dropping need table ownership.
- **What it does, in one transaction:** `SHARE` locks the month's partitions (only writes to that month wait), exports each with `COPY` as CSV into `<dir>/<group>-<month>-<id>/` with a SHA-256 per file, writes `manifest.json` (format `omg-archive-v1`, the migration lineage, exact sums of reservations and ledger) and `manifest.json.sha256`, records `archived_partitions` and, for history, each archived day's exact budget contribution per workspace and key (`archived_budget_contributions`, so `budget verify` still reconciles lifetime and monthly totals), then takes the parents' locks in admission order (bounded by `lock_timeout`) and detaches the partitions into schema `omg_archive` (outside `public`; no runtime access), or drops them with `--drop`. On failure it rolls back and removes the export directory.
- **After archival:** usage of the month is still answered from its rollups; totals are unchanged; prices, the remaining ledger and audit history are untouched; `async_jobs` and `realtime_responses` keep referencing the archived ids (their insert-time existence checks replaced foreign keys). Keep exports encrypted and retained per your compliance policy; detached tables in `omg_archive` are included in `pg_dump` backups until dropped.

## MultiXacts and history parent checks

When several transactions hold a shared row lock on the same row at once, PostgreSQL records the set of lockers as a **MultiXact**: an id from a 32-bit counter plus one *member* entry per locker in a separate 32-bit member space (`pg_multixact/members`). Like transaction ids, both wrap around: autovacuum must freeze old MultiXacts (`autovacuum_multixact_freeze_max_age`, default 400 M ids) and keeps member usage below about 2^31 by forcing aggressive anti-wraparound vacuums as members are consumed. A workload that creates members quickly therefore causes frequent whole-table freezing vacuums and pressure on the MultiXact SLRU caches; in the worst case PostgreSQL stops assigning new MultiXacts (and writes fail) until vacuum catches up.

**Where they came from.** A foreign-key check on `INSERT` locks the referenced row `FOR KEY SHARE`. Before `0034` every admission inserted an execution referencing its workspace, key, deployment and cost center, and a reservation referencing its price version, so concurrent admissions shared locks on the same few rows (the busiest deployment and price version above all). P6 measured a median 1.3 and up to 5.4 MultiXact members per successful request, 2.6–10.9 % of the 2^32 member space per day at 1,000 requests/s (gate D3: about 1 %). `0034` removes those keys (see [governance](governance.md#history-parent-checks)); scoped admission takes no shared row locks of its own, so the request path creates no MultiXacts. `GATEWAY_ADMISSION_MODE=global` (the rollback protocol) still reads keys, workspaces and catalog rows `FOR SHARE` and does create them.

**Monitoring.** Track these with the PostgreSQL exporter or a periodic query; alert well before the defaults force aggressive vacuums:

```sql
-- Age of the oldest MultiXact still referenced, per database (alert above ~200 M;
-- autovacuum forces freezing at autovacuum_multixact_freeze_max_age, 400 M).
SELECT datname, mxid_age(datminmxid) FROM pg_database ORDER BY 2 DESC;
-- Per table: which relations hold old MultiXacts (vacuum these first).
SELECT c.oid::regclass, mxid_age(c.relminmxid) FROM pg_class c WHERE c.relkind IN ('r','m','t') ORDER BY 2 DESC LIMIT 10;
-- Creation rate: next MultiXact id and member offset at the last checkpoint
-- (superuser or pg_read_all_stats). Sample twice; the member delta per
-- second x 86,400 / 2^32 is the share of the member space used per day.
SELECT next_multixact_id, next_multi_offset, checkpoint_time FROM pg_control_checkpoint();
-- SLRU pressure: reads and misses of the MultiXact caches (PostgreSQL 17 names
-- them 'multixact_offset' and 'multixact_member'); rising blks_read means the
-- caches are too small (multixact_member_buffers / multixact_offset_buffers).
SELECT name, blks_hit, blks_read, blks_written FROM pg_stat_slru WHERE name LIKE 'multixact%';
```

With `0034` in scoped mode, expect members per request near zero on the request path (measured below); remaining sources are management transactions (catalog and price edits, which serialize) and operator SQL. If members per day exceed about 1 % of 2^32, find the hot rows with `pgrowlocks` on the suspected tables during load, and check that the `0034` triggers are present (`history verify` reports `enforcement_missing:*` otherwise).

**`history verify`.** Executions and reservations are checked at insert by trigger; the verifier re-checks stored history:

```sh
open-model-gateway history verify                    # all history, one REPEATABLE READ snapshot
open-model-gateway history verify --since 2026-10-01T00:00:00Z
```

It prints JSON (`executions_checked`, `reservations_checked`, `finding_count`, `findings` with up to five example ids per check) and exits nonzero on any finding. Checks: `execution_key_not_in_workspace`, `execution_workspace_missing`, `execution_deployment_missing`, `execution_cost_center_missing`, `execution_batch_job_not_in_workspace`, `reservation_price_not_of_deployment`, and `enforcement_missing:<trigger or key>` (a disabled or missing check/guard trigger, or a missing or unvalidated reservation → execution / ledger → reservation key). It reads every partition (about 4 s for 2 M and 30 s for 10 M attempts on the load-test laptop), so run full scans off-peak. `serve` runs it under the `history_verify` work lease every hour over the history admitted since the previous run (minus one hour of overlap; the last 25 hours on first start) and exports `gateway_history_verify_findings`. Any finding opens the built-in critical installation incident **Request history references missing or mismatched parents** (`history_orphans`; Platform Admins get email). A finding means an owner-level session wrote around the triggers (for example with `session_replication_role=replica` or a disabled trigger) or removed a parent: do not delete history; restore the parent or the triggers from a backup and investigate. Only a full `history verify` with no finding resolves the incident.

## Load test baseline

`apps/gateway/tests/load_test.rs` is a reproducible, ignored harness with no paid calls. For real gateway processes, several replicas and PgBouncer, see the [multi-replica harness](#multi-replica-load-test-harness) and the [capacity baseline](#capacity-baseline). An in-process mock adapter replaces the upstream: about 20 ms per non-stream response, and four deltas for streams. Everything else runs for real: authentication, routing, durable admission (installation lock, rate and budget policies), settlement and the ledger, against PostgreSQL over a real TCP listener. The harness creates and drops its own `omg_load_<random>` database on a loopback server.

```sh
DATABASE_URL=postgres://gateway:gateway@127.0.0.1:54339/gateway \
  cargo test --release -p open-model-gateway --features integration-tests \
  --test load_test -- --ignored --nocapture
# LOAD_TEST_REQUESTS (1000), LOAD_TEST_CONCURRENCY (50,200), LOAD_TEST_UPSTREAM_MS (20),
# LOAD_TEST_POOL (10), LOAD_TEST_MAX_CONCURRENT (256), LOAD_TEST_PRESEED (0)
# Example used below: LOAD_TEST_PRESEED=200000 LOAD_TEST_REQUESTS=500 LOAD_TEST_CONCURRENCY=50
```

After each scenario the harness asserts these ledger invariants:

- No pending or unknown reservations remain.
- Every execution has exactly one reservation, one hold and one settlement.
- The hold sum equals 120 µUSD × successes, and the settled sum equals 11 µUSD × successes, exactly. The ledger matches the reservations.
- Denied requests never executed, and upstream calls equal admitted attempts.
- In the budget scenario, settled spend never exceeds the budget.
- `gateway_settlements_total{outcome="settled"}` matches the count, with no `held` settlements.
- `budget verify` reports every maintained budget-totals bucket equal to a full scan.

Results (2026-10-09; Apple Silicon laptop, Docker PostgreSQL 17 on 54339, release build, 10-connection pool; half of the requests stream). Latency is client-observed full response time, in milliseconds. "Before" is the previous release (budget windows scanned), and "after" uses maintained budget totals (`0015_budget_totals.sql`). Both were measured on the same machine in the same session:

| Scenario | Requests | Before req/s (p50 / p99) | After req/s (p50 / p99) | Errors |
|---|---|---|---|---|
| mixed, 50 concurrent | 1000 | 113 (434 / 576) | 125 (395 / 461) | 0 |
| budget contention, 50 concurrent | 1000 | 97 (483 / 702) | 123 (363 / 534) | 490 ok / 510 clean 429 `budget_exceeded`; 5390 of 5500 µUSD settled |
| mixed, 50 concurrent, 200k history rows this month | 500 | **19** (2574 / 2724) | **124** (392 / 526) | 0 |
| budget contention, 50 concurrent, 200k history rows | 500 | **20** (2436 / 2655) | **150** (280 / 467) | 240 ok / 260 clean 429 |
| mixed, 200 concurrent, 200k history rows | 1000 | n/a | 116 (1697 / 1802) | 0 |
| budget contention, 200 concurrent, 200k history rows | 1000 | n/a | 124 (1409 / 2041) | 490 ok / 510 clean 429 |

After each run, the harness also runs `budget verify`. It found 55 buckets consistent with the full scan, which took 1.04 s over 200k rows.

Findings:

1. **Fixed: pool starvation under concurrency.** Before the fix, at 200 concurrent requests on a 10-connection pool, transactions waiting for the installation lock held every pooled connection. Authentication timed out (105 × 503 out of 1000), and 40 streams could not be finalized (settlement `held`, stream ended with an error rather than `[DONE]`). Admission and settlement now queue in process (`governance::LockGates`). At most a quarter of the pool waits on the lock per queue, and settlements never queue behind admissions. Repeated runs now show zero errors. The pool size is configurable through `GATEWAY_DATABASE_MAX_CONNECTIONS`.
2. **Throughput ceiling: about 115 to 150 req/s per installation in this environment.** Admission and settlement serialize on the installation row by design, so this ceiling is shared by all replicas. Adding replicas or connections does not raise it. Latency at higher concurrency is queueing (Little's law), not failure.
3. **Fixed: budget checks no longer scale with history.** Before, each budget check aggregated every reservation in its window (plus the legacy execution anti-join) while holding the lock. With 200k reservations this month the installation check took about 59 ms, and throughput fell to 19 req/s. Admission now reads `budget_totals`: one indexed lookup for all budget layers, O(layers). Statement-level triggers maintain the table in the same transaction as every reservation and execution write. Rate and concurrency limits read only the current minute and live leases. With 200k history rows, throughput matches the no-history case. Monitoring, policy and alert *reports* outside admission (`alerts`, usage pages) still aggregate their own windows; since P1 they no longer take the installation lock either (see [read-path results](#read-path-results-p1)).

## Multi-replica load-test harness

The in-process harness above runs one gateway inside the test binary. The multi-replica harness (scale plan phase P0) runs real gateway processes against PostgreSQL and PgBouncer, so it measures what an operator would deploy. It is test-only: no provider is ever called, and it only creates throwaway databases named `omg_loadtest*`.

| Part | What it does |
|---|---|
| `tools/mock-upstream` | Deterministic OpenAI-compatible upstream (`POST /v1/chat/completions`, complete and streamed). Configurable latency, time to first token, completion tokens, inter-token delay and a deterministic error rate. Usage is exact (12 prompt tokens, N completion tokens, a complete cache split), so every attempt settles at a known cost. It stores no prompt text, only a `nonce:<id>` marker, and lists its calls at `GET /__mock/calls`. |
| `tools/loadgen run` | Open-loop generator: request *i* is sent at `start + i/rate` whatever earlier requests are doing, so overload shows up as latency and errors instead of a silently lower rate. It round-robins over several gateway URLs, spreads requests over many seeded keys (and therefore workspaces), mixes streamed and complete requests, and measures latency from the scheduled time. Each prompt carries a nonce; the gateway's `x-request-id` (its `root_request_id`) is recorded per request. |
| `tools/loadgen seed` | Generates users, personal and shared workspaces, service accounts, keys, the mock catalog, policies and optional settled history server-side with set-based SQL (`tools/loadgen/seed.sql`). Keys are derived from a seed string by both the seeder and the generator, so no token is printed or stored. It refuses any database not named `omg_loadtest*` (in the client and in SQL), and must connect as the schema owner. |
| `deploy/loadtest/compose.yaml` | Compose project `omg-loadtest`: PostgreSQL 17 (tuned, `pg_stat_statements`), PgBouncer 1.25 in transaction mode, three gateway replicas with metrics listeners, the mock upstream at a pinned private address (`GATEWAY_LOCAL_UPSTREAMS`), and the generator. Images come from `mirror.gcr.io`, `ghcr.io` and the local `deploy/loadtest/Dockerfile` build; nothing from Docker Hub. The gateways use the restricted runtime role and `deploy/staging/runtime-grants.sql`. |
| `scripts/loadtest.py` | Runner: `up`, `reset-db`, `migrate` (upgrade an existing seeded database with the image's migrations), `seed`, `gateways --replicas N --via pgbouncer\|direct`, `run`, `report`, `baseline`, `down`. Passwords are generated into `.local/loadtest/stack.env`; results are JSON files in `.local/loadtest/results/`. |

After every run, `loadgen` checks, per request id:

- every successful request reached the upstream exactly once (no implicit retries); denied requests never did; and no upstream call lacks a client request;
- every successful request has exactly one execution, one settled reservation, one hold and one settlement ledger entry, with the mock's exact usage, the exact cost (20 µUSD each at the seeded price) and ledger sums equal to the reservation sums;
- denied requests have no execution, nothing admitted during the run is still pending, and no execution lacks a reservation.

The runner then runs `open-model-gateway budget verify` and `history verify` (on images that have it), saves the top statements from `pg_stat_statements` and PgBouncer's `SHOW STATS`, and fails the run on any violation. The JSON report also contains client latency (all, streamed, complete), time to first byte, gateway overhead (client service time minus the mock's own service time for the same nonce), per-replica counts, and the `gateway_admission_seconds` / `gateway_settlement_seconds` deltas summed over all replicas.

```sh
python3 scripts/loadtest.py up
python3 scripts/loadtest.py reset-db
python3 scripts/loadtest.py seed --users 5000 --shared 2000 --keys 20000 --history 0
python3 scripts/loadtest.py gateways --replicas 3 --via pgbouncer
python3 scripts/loadtest.py run --label 3r-pgb-60 --rate 60 --duration 45 --warmup 5
python3 scripts/loadtest.py report
python3 scripts/loadtest.py down          # removes containers, network, data volume and the image
```

A second, isolated stack (for example two agents measuring at once) uses `--project omg-loadtest-<name> --slot 1-9` on every command: its own containers, data volume, image tag (`<project>:local`), state directory (`.local/<project>`), host ports (PostgreSQL 54369+100×slot, gateways 18301+10×slot…) and private subnet (10.213.(47+slot).0/24). `down` only removes the selected project.

`run --readers N` adds N concurrent closed-loop management readers for the duration of the run: platform cost report, usage overview and request log, a shared workspace's usage overview and requests, `/me/summary`, `/me` and `/v1/models`, over the last `--reader-days` (default 7) days. They use the seeded reader: user 1 is a platform Auditor that owns shared workspace 1, with a browser session whose cookie is derived from the seed like the keys. The JSON report gains `reader` (per-endpoint statuses and latency), and `report` adds a readers column.

`tools/loadgen/tests/end_to_end.rs` runs the same seed, real gateway (with the real `openai_compatible` adapter), mock, generator and verification in one process against a throwaway database on `DATABASE_URL`, as part of `cargo test --workspace --all-features`.

The generator, mock, gateways and PostgreSQL share one machine in the laptop stack, so they compete for CPU. For certification runs (scale plan P8) put the generator and mock on separate hosts.

## Capacity baseline

Measured 2026-10-09/10 at commit `e05035f` plus the P0 changes (no admission changes), with `scripts/loadtest.py baseline`.

- **Hardware:** Apple M4 Pro (12 cores, 48 GB), macOS 26.6. Docker Desktop 29.8.1 VM with 12 vCPUs and 7.7 GiB. Every container, including the generator and mock, ran in that VM.
- **PostgreSQL 17 (alpine):** `shared_buffers=1GB`, `effective_cache_size=3GB`, `work_mem=16MB`, `max_wal_size=8GB`, `wal_compression=lz4`, `random_page_cost=1.1`, `jit=off`, `synchronous_commit=on` (default), `pg_stat_statements` and `track_io_timing` on.
- **Gateways:** release build, `GATEWAY_DATABASE_MAX_CONNECTIONS=10` and `GATEWAY_MAX_CONCURRENT_REQUESTS=128` per replica, `info` logging.
- **PgBouncer 1.25.0:** transaction mode, `default_pool_size=40`, `max_prepared_statements=200`.
- **Data:** 5,000 users, 7,000 workspaces (2,000 shared), 20,000 keys; an installation rate policy, an installation monthly budget and workspace-type monthly budgets, so every admission evaluates the rate layer and budget totals. "Seeded" adds 2,000,000 settled historical attempts over 120 days (plus 1,000 unknown last month): 2 M executions, 2 M reservations, 4 M ledger entries, 2.7 M budget-total buckets, a 3.8 GB database. Seeding took 391 s.
- **Traffic:** 45 s per run (the first 5 s excluded), 2,000 hot keys, half streamed, `max_completion_tokens` 16. The mock answers complete requests after 50 ms and streams 4 tokens (30 ms to the first, then 5 ms apart).

Overhead is client time minus mock time. Admission, lock wait (`phase="locks"`) and settlement come from the gateway histograms (bucket-interpolated). 429s are `rate_limit_error` from the per-replica capacity limit.

| History | Replicas | DB path | Offered/s | OK/s | Client p50 / p95 / p99 ms | Overhead p50 / p99 ms | Admission p50 / p99 ms | Lock wait p50 / p99 ms | Settlement p50 / p99 ms | 429s |
|---|---|---|---|---|---|---|---|---|---|---|
| empty | 1 | PgBouncer | 60 | 59.9 | 64 / 68 / 70 | 11 / 15 | 6.3 / 12.7 | 0.2 / 0.4 | 2.0 / 5.9 | 0 |
| empty | 1 | direct | 60 | 59.9 | 63 / 67 / 69 | 10 / 14 | 6.9 / 12.7 | 0.1 / 0.2 | 1.3 / 5.3 | 0 |
| empty | 3 | PgBouncer | 60 | 59.9 | 65 / 374 / 481 | 11 / 426 | 7.5 / 374 | 0.2 / 98 | 2.8 / 186 | 0 |
| empty | 3 | direct | 60 | 59.9 | 67 / 72 / 85 | 14 / 31 | 9.5 / 25 | 0.1 / 1.6 | 1.4 / 19 | 0 |
| empty | 1 | PgBouncer | 250 | 79.6 | 1,591 / 2,038 / 2,070 | 1,539 / 2,017 | 1,372 / 3,213 | 14.5 / 25 | 85 / 202 | 7,059 |
| empty | 1 | direct | 250 | 99.1 | 1,284 / 1,743 / 1,783 | 1,232 / 1,731 | 1,113 / 2,582 | 11.2 / 25 | 115 / 381 | 6,589 |
| empty | 3 | PgBouncer | 250 | 83.0 | 4,450 / 5,947 / 6,098 | 4,401 / 6,047 | 4,139 / 6,505 | 57 / 102 | 99 / 203 | 6,775 |
| empty | 3 | direct | 250 | 69.7 | 4,950 / 6,610 / 6,705 | 4,898 / 6,653 | 4,593 / 6,514 | 68 / 102 | 143 / 394 | 6,965 |
| 2 M | 1 | PgBouncer | 60 | 59.9 | 67 / 85 / 163 | 14 / 108 | 9.5 / 59 | 0.2 / 18 | 2.4 / 32 | 0 |
| 2 M | 1 | direct | 60 | 59.9 | 65 / 72 / 86 | 12 / 32 | 8.3 / 25 | 0.1 / 1.6 | 2.1 / 17 | 0 |
| 2 M | 3 | PgBouncer | 60 | 59.9 | 66 / 453 / 568 | 13 / 516 | 9.9 / 441 | 0.2 / 101 | 2.8 / 197 | 0 |
| 2 M | 3 | direct | 60 | 59.9 | 66 / 85 / 121 | 13 / 58 | 9.8 / 26 | 0.1 / 20 | 1.6 / 25 | 0 |
| 2 M | 1 | PgBouncer | 250 | 63.5 | 1,887 / 2,875 / 2,886 | 1,835 / 2,834 | 1,708 / 3,245 | 17.5 / 40 | 135 / 392 | 7,722 |
| 2 M | 1 | direct | 250 | 86.1 | 1,423 / 2,214 / 2,433 | 1,371 / 2,380 | 1,273 / 3,198 | 12.1 / 26 | 73 / 202 | 6,957 |
| 2 M | 3 | PgBouncer | 250 | 67.9 | 4,745 / 7,174 / 7,364 | 4,694 / 7,311 | 4,759 / ≥6,554 | 67 / 158 | 140 / 401 | 7,322 |
| 2 M | 3 | direct | 250 | 72.4 | 4,928 / 7,145 / 7,356 | 4,877 / 7,301 | 4,575 / ≥6,554 | 63 / 109 | 153 / 402 | 7,023 |

Every run passed every invariant: each successful request matched exactly one upstream call and one settled reservation with exact sums, no denied request executed, nothing stayed pending, and `budget verify` was consistent (0.3 s on the empty database, 21 s over 2.7 M buckets on the seeded one). "≥6,554" means the top bucket of the histogram build used for the baseline (later builds extend the buckets to 52 s).

Findings:

1. **The installation-wide ceiling on this laptop is about 65–100 successful requests/s, and replicas do not raise it.** Three replicas never beat one at overload; they only queue more work (3 × 128 permits), so p50 latency goes from 1.3–1.9 s to 4.4–4.9 s. Under overload the in-process `queue` phase is almost all of the admission time. A shorter 25 s probe reached 120/s; throughput falls as each UTC minute fills up (next finding).
2. **The critical section is about 10 ms per admission at overload, and most of it is per-minute rate accounting.** At 250/s offered (1 replica, empty) the serialized phases were `locks` 14.5 ms p50 (waiting for the previous holder), `limits` 7.9 ms, `write` 1.2 ms, `read` 1.2 ms and `commit` 0.25 ms. The `RATE_ACCOUNTING` statement averaged 6.2 ms and the installation row lock statement accumulated 137 s of waiting in 45 s. This matches the scale design's §2.1/§2.3 analysis: removing the lock alone is not enough; the per-minute scan must become a maintained counter (P2).
3. **History still costs admission time.** With 2 M historical attempts, rate accounting averaged 4.8 ms instead of 3.0 ms at 60/s, the `limits` phase 6.5 ms instead of 3.9 ms, and overload throughput through PgBouncer fell from 80 to 64/s.
4. **Below saturation, admission meets the design target and settlement is cheap:** admission p50 6–10 ms, p99 13–26 ms; settlement p50 1.3–2.8 ms; gateway overhead p50 10–14 ms (two HTTP hops, authentication, routing, admission and settlement).
5. **Three replicas through PgBouncer showed p95 spikes (374–453 ms) at 60/s** that the direct path did not, with lock-wait p99 around 100 ms. A 30 s repeat on the seeded database had p95 70 ms and p99 182 ms, and PgBouncer reported an average client wait of 0 µs and few server-side re-parses, so pooling is not the cause. The likely cause is periodic installation-lock work multiplied by replicas (lifecycle cleanup, reconciliation and the alert evaluator on every replica, design §2.6). The P0 histograms do not cover those jobs; this is not yet proven.
6. **With `max_prepared_statements=0`, PgBouncer breaks the gateway (decision gate D4):** 98 of 100 requests failed (`503 accounting_unavailable`/`api_error` and some 401s from failed authentication lookups), with no ledger damage. With 200 it worked in every run.

### Read-path results (P1)

Measured 2026-10-10 on the same machine and stack, seeded database (2 M history attempts, 3.8 GB), via PgBouncer, 45 s runs. "Readers" are two concurrent closed-loop management readers (`run --readers 2`, 7-day window): platform cost report, usage overview and request log, a shared workspace's usage overview and requests, `/me/summary`, `/me` and `/v1/models`. The heavy queries take 1.0–1.5 s each on this dataset. "Before" is the gateway at commit `1d9d1c9` (reports, usage, logs and `/me` hold the installation lock); "after" is P1.

| Replicas | Offered/s | Readers | OK/s before → after | Client p50 / p99 ms before → after | Lock wait p99 ms before → after | Reader p50 ms before → after |
|---|---|---|---|---|---|---|
| 1 | 60 | 0 | 59.9 → 59.2 | 70 / 228 → 70 / 529 | 25 → 25 | – |
| 1 | 60 | 2 | 2.3 → 55.6 | 20,542 / 40,316 → 134 / 2,772 | 2,384 → 47 | 950 → 9 |
| 1 | 250 | 2 | 2.2 → 48.5 | 21,769 / 42,296 → 2,004 / 9,401 | 2,372 → 173 | 994 → 10 |
| 3 | 60 | 2 | 8.4 → 45.6 | 29,972 / 41,169 → 94 / 10,498 | 1,637 → 362 | 960 → 16 |
| 3 | 250 | 2 | 6.4 → 48.0 | 22,188 / 41,047 → 7,143 / 8,259 | 1,614 → 205 | 988 → 9 |

- **Before P1, two dashboard readers stopped inference.** Each heavy report held the installation lock for its whole query, so two readers kept it busy almost continuously: admission waited about 25 s (p50) and settlements timed out (`503 accounting_unavailable`, incomplete streams), leaving 12–26 reservations pending per run. The gateway failed closed (holds kept, `budget verify` consistent), but the harness's "nothing pending" invariant failed in three of the four runs.
- **After P1 the readers take no lock.** Inference throughput with readers rose 6–24× (55.6 ok/s at 60/s offered on one replica), every invariant held, and the cheap read endpoints answer in about 10 ms instead of queueing behind reports.
- **What remains is database CPU contention on a serialized admission path.** On this single shared VM the readers' scans slow every statement in the still-global admission section (the installation-lock statement averaged 14–64 ms with readers, 3–6 ms without), and that becomes queueing (p99 seconds, some capacity 429s). Shortening the admission section (scale plan P2/P3) and moving report scans to a [reporting replica](#read-snapshots-and-the-reporting-replica) address it. The replica path is covered by integration tests but was not part of this laptop run, which has no standby.
- The no-reader control is unchanged within run-to-run noise.

### Admission results (P2: maintained counters, shorter critical section)

Measured 2026-10-10 on the same laptop and settings, in an isolated copy of the harness. Because the installation policy layer is being removed, the seed's installation rate limits were moved to workspace-type defaults and every key lineage (installation and type monthly budgets unchanged). "Before" is the previous release with that same policy setup; "after" adds `0024_rate_counters.sql`, `0025_budget_totals_detail.sql` and the shorter critical section (price and reservation bounds resolved under the shared catalog lock before the installation lock; one-statement revalidation, deployment check with clock, limit read, and execution/reservation/hold write; 6 round trips under the installation lock instead of about 14). Successful requests per second at overload:

| History | Replicas | DB path | Offered/s | P0 (installation rate scan) | Before | After |
|---|---|---|---|---|---|---|
| empty | 1 | PgBouncer | 250 / 500 | 79.6 / – | 84.8 / – | 202.9 / 209.1 |
| empty | 1 | direct | 250 | 99.1 | 176.7 | 225.3 |
| empty | 3 | PgBouncer | 250 | 83.0 | 142.6 | 184.5 |
| 2 M | 1 | PgBouncer | 250 / 800 | 63.5 / – | 143.4 | 172.4 / 175.5 |
| 2 M | 1 | direct | 250 | 86.1 | 170.9 | 193.1 |
| 2 M | 3 | PgBouncer | 250 / 800 | 67.9 / – | 136.6 | 161.9 / 160.3 |

The 2 M rows were measured before a final trigger optimization worth about 10% (empty, 1 replica: 182 → 203/s). Every run passed every invariant and `budget verify`. Below saturation (60/s, 1 replica, PgBouncer) admission p50/p99 fell from 4.8/10.0 ms to 2.9/6.4 ms, and the time under the installation lock (`read`+`limits`+`write`+`commit` p50) from 4.5 to 3.1 ms. With 3 replicas through PgBouncer on the seeded database, client p99 at 60/s fell from 188 to 67 ms. Migrating the 2 M-row database to 0025 took 8.6 s, during which history writes are frozen.

Findings:

1. **The ceiling roughly doubled but is still one serializer, about 175–225 successful requests/s.** Replicas still do not raise it. This does not meet decision gate D1 (400/s); scoped admission (P3) remains next.
2. **Fewer round trips matter most through PgBouncer** (85 → 203/s with 1 replica), because every round trip under the lock pays the proxy hop.
3. **Counters trade scan time for trigger time.** With traffic spread over 7,000 workspaces, a scoped per-minute scan cost only 0.04 ms, while the counter triggers add about 0.3–0.5 ms per admission and settlement. The counters remove the growth with a scope's traffic (a 200 req/s workspace scans 12,000 rows per minute) and the installation-wide scan (6.2 ms in P0); a hot-scope run is still to do.
4. **Group commit had no effect yet** (`commit_delay` 200 or 1,000 µs: within 2%), because the installation lock lets only one admission or settlement commit at a time.

### Combined capacity (P1 + P2 + no installation limits)

Measured 2026-10-10 on the same laptop and settings: the full P0 matrix plus two report readers (`run --readers 2`), on an empty and on the 2 M-attempt database. "After P2" is commit `c43e1ae` with type and key rate limits, type monthly budgets and an installation monthly budget, rerun in the same session as a control. "After 0026" is the same code plus `0026_remove_installation_limits.sql` (the same type and key limits and budgets; no installation layer exists). "P0 baseline" is the [capacity baseline](#capacity-baseline) above; its reader rows are the P1 "before" runs, which exist only for the 2 M database. Each cell is successful requests per second and client p99 in ms. All 40 runs passed every ledger invariant and `budget verify`.

| History | Setup | Readers | Offered/s | P0 baseline ok/s · client p99 ms | After P2 (c43e1ae) ok/s · p99 ms | After 0026 ok/s · p99 ms | ok/s change vs P2 |
|---|---|---|---|---|---|---|---|
| empty | 1 replica, PgBouncer | 0 | 60 | 59.9 · 70 | 59.9 · 67 | 59.9 · 65 | +0% |
| empty | 1 replica, PgBouncer | 0 | 250 | 79.6 · 2,070 | 203.2 · 715 | 237.5 · 635 | +17% |
| empty | 1 replica, PgBouncer | 2 | 60 | – | 59.8 · 67 | 59.8 · 64 | +0% |
| empty | 1 replica, PgBouncer | 2 | 250 | – | 204.5 · 728 | 233.3 · 1,038 | +14% |
| empty | 1 replica, direct | 0 | 60 | 59.9 · 69 | 59.9 · 66 | 59.9 · 65 | +0% |
| empty | 1 replica, direct | 0 | 250 | 99.1 · 1,783 | 228.5 · 1,068 | 249.2 · 177 | +9% |
| empty | 3 replicas, PgBouncer | 0 | 60 | 59.9 · 481 | 59.9 · 67 | 59.9 · 67 | +0% |
| empty | 3 replicas, PgBouncer | 0 | 250 | 83.0 · 6,098 | 191.4 · 2,170 | 231.2 · 1,764 | +21% |
| empty | 3 replicas, PgBouncer | 2 | 60 | – | 59.4 · 67 | 58.9 · 66 | -1% |
| empty | 3 replicas, PgBouncer | 2 | 250 | – | 184.3 · 2,171 | 214.1 · 1,952 | +16% |
| 2 M | 1 replica, PgBouncer | 0 | 60 | 59.9 · 163 | 59.9 · 67 | 59.9 · 67 | +0% |
| 2 M | 1 replica, PgBouncer | 0 | 250 | 63.5 · 2,886 | 193.4 · 723 | 222.1 · 625 | +15% |
| 2 M | 1 replica, PgBouncer | 2 | 60 | 2.3 · 40,316 | 58.3 · 144 | 59.1 · 112 | +1% |
| 2 M | 1 replica, PgBouncer | 2 | 250 | 2.2 · 42,296 | 158.9 · 1,042 | 191.4 · 921 | +20% |
| 2 M | 1 replica, direct | 0 | 60 | 59.9 · 86 | 59.9 · 65 | 59.9 · 65 | +0% |
| 2 M | 1 replica, direct | 0 | 250 | 86.1 · 2,433 | 215.7 · 636 | 245.8 · 309 | +14% |
| 2 M | 3 replicas, PgBouncer | 0 | 60 | 59.9 · 568 | 59.9 · 254 | 59.9 · 65 | +0% |
| 2 M | 3 replicas, PgBouncer | 0 | 250 | 67.9 · 7,364 | 182.1 · 2,075 | 202.9 · 1,916 | +11% |
| 2 M | 3 replicas, PgBouncer | 2 | 60 | 8.4 · 41,169 | 58.1 · 191 | 58.3 · 172 | +0% |
| 2 M | 3 replicas, PgBouncer | 2 | 250 | 6.4 · 41,047 | 143.1 · 2,848 | 156.6 · 2,968 | +9% |

Findings:

1. **Removing the installation layer raised the overload ceiling by 9–21 %** in every saturated cell (one replica through PgBouncer: 203 → 238/s empty, 193 → 222/s with 2 M attempts). Below saturation throughput is unchanged and admission p50 fell by 0.1–1.4 ms.
2. **Why:** each reservation or execution write updates 8 `budget_totals` rows instead of 12, and none of them is shared by every request. The admission write and the settlement update became 18–30 % cheaper, which shortens the time under the installation row lock. That lock is still the serializer: it remains the top statement, and three replicas still do not beat one.
3. **One replica on direct connections no longer saturates at 250/s** (249/s, admission p50 3.2 ms, client p99 177 ms).
4. **With two report readers** the gain holds (one replica, 2 M: 159 → 191/s); readers take no lock (P1) and keep their latency.
5. **Against P0**, overload throughput is 2.5–3.5× without readers and 7–87× with readers.
6. The ceiling (about 200–250/s on this machine) is still below decision gate D1 (400/s). Admission no longer needs any installation limit data, so scoped admission (P3) only has to replace the authorization serialization on the installation row.

Upgrading the 2 M-attempt `c43e1ae` database to 0026 in place took 0.9 s; `budget verify` was consistent afterwards (2.73 M buckets, 22 s).

### Scoped admission (P3)

Measured 2026-10-10 on the same laptop and settings after `0027_scoped_admission.sql` ([governance](governance.md#scoped-admission-lock-order)): the combined matrix extended to 500 and 1000/s offered, on an empty and on the 2 M-attempt database. "P3 global mode" is the same image with `GATEWAY_ADMISSION_MODE=global` (the former installation-row protocol), run in the same session as a control; the earlier columns repeat the table above. Pool 10 per replica. 62 of 63 runs passed every ledger invariant and `budget verify`; the exception (2 M, 3 replicas, 2 readers, 500/s) failed closed under overload: 35 settlements hit their deadline (`503 accounting_unavailable`) and 59 reservations stayed pending for lease reconciliation with their holds kept.

| History | Setup | Readers | Offered/s | P0 baseline ok/s · client p99 ms | After P2 (c43e1ae) ok/s · p99 ms | After 0026 ok/s · p99 ms | P3 global mode (same session) ok/s · p99 ms | **P3 scoped** ok/s · p99 ms | P3 vs 0026 |
|---|---|---|---|---|---|---|---|---|---|
| empty | 1 replica, PgBouncer | 0 | 60 | 59.9 · 70 | 59.9 · 67 | 59.9 · 65 | – | 59.9 · 67 | +0% |
| empty | 1 replica, PgBouncer | 0 | 250 | 79.6 · 2,070 | 203.2 · 715 | 237.5 · 635 | 236.3 · 646 | 249.6 · 63 | +5% |
| empty | 1 replica, PgBouncer | 0 | 500 | – | – | – | 241.3 · 597 | 493.6 · 184 | – |
| empty | 1 replica, PgBouncer | 0 | 1000 | – | – | – | – | 714.6 · 210 | – |
| empty | 1 replica, PgBouncer | 2 | 60 | – | 59.8 · 67 | 59.8 · 64 | – | 59.8 · 67 | +0% |
| empty | 1 replica, PgBouncer | 2 | 250 | – | 204.5 · 728 | 233.3 · 1,038 | – | 247.9 · 64 | +6% |
| empty | 1 replica, PgBouncer | 2 | 500 | – | – | – | – | 489.7 · 71 | – |
| empty | 1 replica, direct | 0 | 60 | 59.9 · 69 | 59.9 · 66 | 59.9 · 65 | – | 59.9 · 66 | +0% |
| empty | 1 replica, direct | 0 | 250 | 99.1 · 1,783 | 228.5 · 1,068 | 249.2 · 177 | – | 249.7 · 61 | +0% |
| empty | 1 replica, direct | 0 | 500 | – | – | – | – | 499.3 · 62 | – |
| empty | 1 replica, direct | 0 | 1000 | – | – | – | – | 876.6 · 168 | – |
| empty | 3 replicas, PgBouncer | 0 | 60 | 59.9 · 481 | 59.9 · 67 | 59.9 · 67 | – | 59.9 · 67 | +0% |
| empty | 3 replicas, PgBouncer | 0 | 250 | 83.0 · 6,098 | 191.4 · 2,170 | 231.2 · 1,764 | 216.0 · 1,797 | 249.6 · 583 | +8% |
| empty | 3 replicas, PgBouncer | 0 | 500 | – | – | – | 220.8 · 1,833 | 499.3 · 65 | – |
| empty | 3 replicas, PgBouncer | 0 | 1000 | – | – | – | – | 991.7 · 432 | – |
| empty | 3 replicas, PgBouncer | 2 | 60 | – | 59.4 · 67 | 58.9 · 66 | – | 57.6 · 419 | -2% |
| empty | 3 replicas, PgBouncer | 2 | 250 | – | 184.3 · 2,171 | 214.1 · 1,952 | – | 238.8 · 860 | +12% |
| empty | 3 replicas, PgBouncer | 2 | 500 | – | – | – | – | 474.3 · 895 | – |
| 2 M | 1 replica, PgBouncer | 0 | 60 | 59.9 · 163 | 59.9 · 67 | 59.9 · 67 | – | 59.9 · 67 | +0% |
| 2 M | 1 replica, PgBouncer | 0 | 250 | 63.5 · 2,886 | 193.4 · 723 | 222.1 · 625 | 208.6 · 661 | 249.6 · 63 | +12% |
| 2 M | 1 replica, PgBouncer | 0 | 500 | – | – | – | 217.2 · 661 | 499.3 · 66 | – |
| 2 M | 1 replica, PgBouncer | 0 | 1000 | – | – | – | – | 696.6 · 214 | – |
| 2 M | 1 replica, PgBouncer | 2 | 60 | 2.3 · 40,316 | 58.3 · 144 | 59.1 · 112 | – | 59.3 · 182 | +0% |
| 2 M | 1 replica, PgBouncer | 2 | 250 | 2.2 · 42,296 | 158.9 · 1,042 | 191.4 · 921 | – | 241.9 · 316 | +26% |
| 2 M | 1 replica, PgBouncer | 2 | 500 | – | – | – | – | 453.8 · 448 | – |
| 2 M | 1 replica, direct | 0 | 60 | 59.9 · 86 | 59.9 · 65 | 59.9 · 65 | – | 59.9 · 66 | +0% |
| 2 M | 1 replica, direct | 0 | 250 | 86.1 · 2,433 | 215.7 · 636 | 245.8 · 309 | – | 249.6 · 317 | +2% |
| 2 M | 1 replica, direct | 0 | 500 | – | – | – | – | 499.3 · 64 | – |
| 2 M | 1 replica, direct | 0 | 1000 | – | – | – | – | 835.7 · 185 | – |
| 2 M | 3 replicas, PgBouncer | 0 | 60 | 59.9 · 568 | 59.9 · 254 | 59.9 · 65 | – | 59.9 · 67 | +0% |
| 2 M | 3 replicas, PgBouncer | 0 | 250 | 67.9 · 7,364 | 182.1 · 2,075 | 202.9 · 1,916 | 196.2 · 2,045 | 249.6 · 64 | +23% |
| 2 M | 3 replicas, PgBouncer | 0 | 500 | – | – | – | 192.0 · 2,080 | 499.3 · 74 | – |
| 2 M | 3 replicas, PgBouncer | 0 | 1000 | – | – | – | – | 962.9 · 527 | – |
| 2 M | 3 replicas, PgBouncer | 2 | 60 | 8.4 · 41,169 | 58.1 · 191 | 58.3 · 172 | – | 54.0 · 942 | -7% |
| 2 M | 3 replicas, PgBouncer | 2 | 250 | 6.4 · 41,047 | 143.1 · 2,848 | 156.6 · 2,968 | – | 239.8 · 1,562 | +53% |
| 2 M | 3 replicas, PgBouncer | 2 | 500 | – | – | – | – | 393.2 · 5,173 ✗ | – |

Follow-up with the final per-replica gates (settlement may use 3/10 of the pool; the matrix above used a quarter):

| History | Setup | Pool | Offered/s | ok/s · client p99 ms |
|---|---|---|---|---|
| empty | 1 replica, PgBouncer | 10 | 1000 | 813.8 · 190 |
| empty | 1 replica, direct | 10 | 1000 | 995.3 · 143 |
| empty | 3 replicas, PgBouncer | 10 | 1000 | 998.4 · 131 |
| empty | 1 replica, PgBouncer | 20 | 1000 | 991.1 · 154 |
| empty | 3 replicas, PgBouncer | 20 | 1500 | 1,080.4 · 434 |
| 2 M | 3 replicas, PgBouncer | 20 | 1500 | 1,044.2 · 437 |
| 2 M | 3 replicas, PgBouncer, 2 readers | 10 | 250 | 247.3 · 203 (global mode: 172.5 · 2,449) |

Hot scopes (empty, scoped): one shared workspace with 2,000 keys served 249.6/s at 250 and 206.3/s at 500 offered on one replica (248.7 and 231.0/s on three); one key 249.4/s at 250. Global mode on the hot workspace: 240.9/s. Group commit (`commit_delay=200` µs) changed nothing measurable (3 replicas at 1000/s: 994.4 vs 991.7/s).

Findings:

1. **No installation-wide serializer: three replicas now beat one.** In the same session the global mode stays at 236–241/s (empty) and 209–217/s (2 M) however much is offered; scoped admission serves 494–499/s at 500/s with client p99 66–184 ms, and three replicas 998/s at 1000/s (one replica 814/s). The ceiling on this laptop is now **about 1,050–1,080 ok/s**, about 4.4× the previous 231–250/s and above decision gate D1 (400/s). At that point PostgreSQL used 7.5 of the VM's 12 vCPUs (shared with PgBouncer, three gateways, the mock and the generator) and PgBouncer 82 % of its single core: the machine is the limit, not a lock.
2. **Per replica, the pool is now the limit.** Admission may use half of `GATEWAY_DATABASE_MAX_CONNECTIONS` and settlement three tenths. At pool 10 one replica tops out near 800–1000/s; at pool 20 it served 991/s. Unlike under the installation lock, raising the pool now raises throughput (size PgBouncer and `max_connections` accordingly).
3. **Below saturation, admission costs the same** (mean 3.5–3.9 ms; one extra round trip that locks the workspace's totals and counter rows, about 0.5 ms), and the locks are uncontended (authority locks p99 0.4 ms, row locks p99 1.1–1.6 ms).
4. **One workspace (or key) tops out around 210–250/s.** Its totals rows serialize its admissions and settlements, as the scale design predicted for unsharded scopes; spread very hot service traffic over several workspaces. Per-scope sharding (scale plan P3b) would lift it.
5. **Group commit:** no effect on local NVMe; keep the defaults unless commit latency is high (network storage, synchronous standby).
6. **Report readers** no longer cost inference throughput (1 replica, 2 M, 250/s: 241.9/s; 191.4/s after 0026). One main-matrix series with three replicas and readers on 2 M slowed every statement (a plain ledger insert 0.12 → 4.85 ms) while the report scans ran — machine-wide CPU/IO contention, not lock waits; its rerun was clean (above). Put report scans on a [reporting replica](#read-snapshots-and-the-reporting-replica).

### Caching and background work (P4/P5)

Measured 2026-10-10 on the same laptop and settings after `0028_change_notifications.sql` and `0029_work_leases.sql` ([governance](governance.md#caches-and-change-notifications)): offered 250, 500, 1000 and 1500/s on one and three replicas behind PgBouncer, pool 10 per replica, with and without two report readers, empty and 2 M history. The same-session control is the same image with `GATEWAY_CONFIG_CACHE=off`; the P3 column repeats the scoped results above. 61 of 62 runs passed every ledger invariant and `budget verify`; the exception (empty, 3 replicas, 2 readers, 500/s) ran inside a WAL-triggered checkpoint and failed closed (11 settlement deadlines, holds kept); four repeats after a `CHECKPOINT` were clean (489–499/s, p99 64–71 ms).

| History | Setup | Readers | Offered/s | P3 scoped ok/s · p99 ms | Caches off (same session) | **P4/P5** ok/s · p99 ms |
|---|---|---|---|---|---|---|
| empty | 1 replica | 0 | 500 | 493.6 · 184 | – | 499.3 · 61 |
| empty | 1 replica | 0 | 1000 | 813.8 · 190 | 822.7 · 184 | 998.6 · 88 |
| empty | 1 replica | 0 | 1500 | – | 796.4 · 186 | 1,074.9 · 139 |
| empty | 3 replicas | 0 | 1000 | 998.4 · 131 | 998.3 · 137 | 998.6 · 80 |
| empty | 3 replicas | 0 | 1500 | – | 1,051.3 · 665 | 1,339.6 · 331 |
| empty | 3 replicas | 2 | 500 | 474.3 · 895 | 498.1 · 71 (repeat) | 498.7 · 64 (repeat) |
| 2 M | 1 replica | 0 | 1000 | 696.6 · 214 | 812.1 · 184 | 998.3 · 111 |
| 2 M | 1 replica | 0 | 1500 | – | 747.6 · 402 | 1,026.9 · 152 |
| 2 M | 3 replicas | 0 | 1000 | 962.9 · 527 | 958.2 · 538; 920.8 · 525 (repeat) | 918.5 · 1,739; 998.5 · 322 (repeat) |
| 2 M | 3 replicas | 0 | 1500 | – | 917.8 · 569 | 1,105.1 · 849 |
| 2 M | 3 replicas | 2 | 250 | 239.8 · 1,562 | 203.3 · 779 (repeat) | 232.3 · 3,832; 240.4 · 1,213 (repeat) |

Findings:

1. **One replica now reaches about 1,000/s on a 10-connection pool** (814/s in P3, 823/s with caches off). Pre-admission reads no longer use the pool on a cache hit and admission and settlement each save a price round trip, so settlements stop queueing in process (settlement mean at 1000/s 94 → 4 ms). PgBouncer counted about half as many transactions per successful request.
2. **Laptop ceiling about 1,340 ok/s** on three replicas at pool 10 (1,051/s with caches off; P3 needed pool 20 for 1,080/s). Below saturation latency is unchanged.
3. **Hit rates ~100 %** for keys, candidates and routes once a key has been seen on a replica (61–89 % at 250/s, where each of 2,000 keys reaches each replica only a few times); prices 100 %. In tests, invalidation through LISTEN took 5–10 ms.
4. **Background work runs once.** Across three replicas each 50 s window saw at most one account-lifecycle check and one counter prune, on the lease holder, with no lock taken when nothing was due.
5. **Remaining latency spikes come from checkpoints and report scans, not background jobs.** 46 of 117 spike seconds (p99 over 4× the run median) fell inside WAL- or time-triggered spread checkpoints; most others were in report-reader runs or at saturation. None appeared in no-reader runs at 500/s or less, apart from the row-lock-bound hot workspace. The spikes seen with three replicas in P0/P1 had a background-job share that is gone: every replica used to run lifecycle cleanup and lease reconciliation under the installation row and evaluate alerts on its own tick. Size `max_wal_size` so checkpoints are time-triggered, and put report scans on a reporting replica.
6. **Hot workspace and hot key unchanged** (about 200–250/s): the scope's totals rows are the limit (scale plan P3b).

### Partitioned history (P6)

Same stack and matrix as P4/P5 (project `omg-loadtest-p6`; pool 10 per replica; 45 s runs; ok/s · client p99 ms), now with history partitioned by month, hourly rollups maintained by the leased job, and a `CHECKPOINT` after every run to read MultiXact counters. 53 runs plus 4 D3 runs; invariants and `budget verify` passed in all, no deadlocks.

| History | Setup | Offered/s | P4/P5 | P6 |
|---|---|---|---|---|
| empty | 1 replica | 1000 / 1500 | 998.6 · 88 / 1,074.9 · 139 | 961.7 · 173 / 974.5 · 153 |
| empty | 3 replicas | 1000 / 1500 | 998.6 · 80 / 1,339.6 · 331 | 998.5 · 289 / 1,162.1 · 392 |
| 2 M | 1 replica | 1000 / 1500 | 998.3 · 111 / 1,026.9 · 152 | 945.8 · 158 / 890.7 · 185 |
| 2 M | 3 replicas | 1000 / 1500 | 918.5 · 1,739 / 1,105.1 · 849 | 998.5 · 88 / 1,187.2 · 390 |
| 10 M | 1 replica | 1000 / 1500 | – | 949.3 · 160 / 923.2 · 192 |
| 10 M | 3 replicas | 1000 / 1500 | – | 998.5 · 82 / 1,141.2 · 413 |
| hot workspace | 1 / 3 replicas | 500 | 202.3 · 734 / 221.1 · 2,128 | 161.4 · 874 / 160.6 · 2,579 |

- **History size no longer matters for admission:** 10 M attempts (21 GB) serve like 2 M. `budget verify` took 24 s on 2 M and 84–92 s on 10 M; the rollup catch-up of 10 M attempts (2,881 hours) took 928 s.
- **Saturation cost:** at 1,500/s offered one replica lost 9–13 % and three replicas on an empty database 13 %; one hot workspace or key fell from about 202–250/s to 160–206/s. The admission and settlement statements on the partitioned tables take 5–15 % longer (foreign-key checks against partitioned parents, per-partition index probes for lookups by id), which matters most where requests serialize on one scope's totals rows.
- **Decision gate D3 failed.** Foreign keys from history to hot parent rows (workspace, key, deployment, price, cost center) create MultiXacts under concurrency: a median 1.3 and up to 5.4 MultiXact members per successful request (2.6–10.9 % of the 2^32 member space per day at 1,000 requests/s; the gate is 1 %). With those keys dropped in an experiment on the 10 M database, three replicas at 1,000/s created none (42,712 MultiXacts and 155,005 members with them). Fixed by `0034` ([P6b](#history-parent-checks-p6b)).

### History parent checks (P6b)

Same stack, cells and settings as P6 (project `omg-loadtest-p6b`; 45 s runs; ok/s · client p99 ms), with `0034` (history parent checks without row locks, partition-pruned settlement in one statement). A same-session control on the P6 image reproduced the regressions first. Every run passed the ledger invariants, `budget verify` and `history verify`; no deadlocks. The 2 M database was vacuumed before its cells (its 2.6 M-row legacy partition otherwise carried up to 435 k dead tuples, which made the expired-lease read 3× slower and one replica 4–7 % slower in an unvacuumed pass).

| Cell | P4/P5 | P6 | P6 control | P6b |
|---|---|---|---|---|
| empty, 1 replica, 1500/s | 1,074.9 · 139 | 974.5 · 153 | 989.4 · 152 | 1,059.6 · 138 |
| empty, 3 replicas, 1500/s | 1,339.6 · 331 | 1,162.1 · 392 | 1,247.6 · 347 | 1,342.8 · 329 |
| 2 M, 1 replica, 1000 / 1500/s | 998.3 · 111 / 1,026.9 · 152 | 945.8 · 158 / 890.7 · 185 | 981.0 · 148 / 967.5 · 153 | 996.3 · 135 / 1,035.4 · 153 |
| 2 M, 3 replicas, 1500/s | 1,105.1 · 849 | 1,187.2 · 390 | 1,198.1 · 370 | 1,270.1 · 344 |
| hot workspace, 1 replica, 250 / 500/s | 249.7 · 334 / 202.3 · 734 | 165.1 · 858 / 161.4 · 874 | 178.2 · 822 / 169.8 · 828 | 249.7 · 369 / 255.3 · 588 |
| hot workspace, 3 replicas, 250 / 500/s | 248.7 · 1,277 / 221.1 · 2,128 | 172.6 · 2,495 / 160.6 · 2,579 | 162.7 · 2,483 / 160.3 · 2,900 | 243.0 · 1,757 / 242.8 · 1,824 |
| hot key, 3 replicas, 250/s | 246.8 · 1,325 | 206.7 · 2,406 | 219.6 · 1,918 | 242.0 · 1,888 |

- **D3 passes.** MultiXacts created in every P6b run: **0** (0 members per request; P6 control in the same session: median 1.65, up to 5.2 members per request, up to 10.5 % of the member space per day at 1,000/s). On the 10 M database with three replicas at 1,000/s: 998.5/s with 0 MultiXacts (control: 998.4/s with 39,330 MultiXacts and 119,836 members, 5.4 %/day).
- **Hot scopes recovered and exceed P4/P5 at overload** (one replica 255/s at 500 offered, was 202/s): the 0034 change removes a lock per parent per insert, and settlement now holds the scope's totals rows for one statement instead of three, each touching only the admission month's partition. At 250/s offered three replicas reach 242–243/s (P4/P5 247–249, within this laptop's run-to-run spread of about ±5 %).
- **Saturation is back at P4/P5 levels** (−1 % to +15 % per cell).
- **Upgrade cost:** `0034` took 35 ms on the 2 M and 43 ms on the 10 M database (metadata only). `history verify` (full) took about 4 s on 2 M and 27–37 s on 10 M attempts; the hourly job reads one window.

### Per-scope admission ceiling

Scoped admission removed every installation-wide serializer, but one workspace (and one key lineage) still serializes its own admissions and settlements on its `budget_totals`, minute and in-flight counter rows: that is what keeps its budgets exact. On the laptop stack one scope tops out at about **200–255 admissions/s** (P3, P6b) however many replicas run, while the installation serves 1,000–1,340/s. Splitting one scope's rows into shards (scale plan P3b) is deferred; until then, make the ceiling visible and spread very hot service traffic over several workspaces or keys.

- **Metrics (per replica, no ids).** `gateway_scope_lock_wait_seconds{lock="rows",path="admission"}` is the time admissions wait for other transactions of the same scope; `gateway_scope_lock_waiting` counts transactions waiting now. Below the ceiling `rows` waits stay around 1–2 ms (p99); a scope at its ceiling pushes them to tens or hundreds of ms. Installation-wide quantiles only move when the hot scope carries a noticeable share of the traffic (the example `GatewayScopeLockWaitHigh` rule in `deploy/monitoring/prometheus-rules.yml` fires on p95 > 100 ms for 10 minutes); the alert rule below finds a single hot scope among many cold ones.
  ```promql
  histogram_quantile(0.95, sum by (le) (rate(gateway_scope_lock_wait_seconds_bucket{lock="rows",path="admission"}[5m])))
  sum(gateway_scope_lock_waiting)
  ```
- **Which scope: the `admission_ceiling` alert rule** (Admin › Settings › Alerts, installation only; [alerts](alerts.md#rule-kinds)). Per workspace and key lineage over the last 5–10 complete minutes it fires when admissions stayed at or above a rate (for example 200/s) in every minute, or when the lock-wait p95 reached a threshold (for example 250 ms). The admission counts come from `rate_minute_counters`. The lock waits come from `admission_lock_waits` (migration 0036): each replica counts, per minute and scope, the committed interactive admissions whose scope locks took at least 10, 25, 50, … 2500 ms (fast admissions are not recorded, at most 20,000 scope-minutes are buffered) and adds them every 5 seconds; maintenance deletes rows older than an hour. The incident names a Team or Project for Platform Admins only and a personal workspace only as "a personal workspace"; email goes to Platform Admins only.
- **What to do.** A workspace at its ceiling: move independent workloads (service accounts, batch-like traffic) to their own workspace or project; one key at its ceiling: issue several keys (each key lineage is its own scope, but they still share the workspace's rows, so the workspace ceiling remains). Raising pool size or replicas does not help a single scope.

## PostgreSQL settings for hot rows and group commit

- **Hot rows.** `budget_totals`, `rate_minute_counters` and `inflight_counters` are updated on every admission and settlement. Only their primary keys are indexed, and no indexed column is ever updated, so updates stay HOT (in-page) when there is free space: `budget_totals` uses `fillfactor=50`, the counter tables `fillfactor=70`. The migrations set threshold-driven autovacuum on all three (`autovacuum_vacuum_scale_factor=0`, `autovacuum_vacuum_threshold=1000`, `autovacuum_vacuum_cost_limit=2000`, `autovacuum_vacuum_cost_delay=1`), so vacuum frequency does not shrink as the tables grow. HOT pruning needs the global xmin to advance: avoid long transactions on the primary (run `budget verify` and reports on a replica where possible). Watch `n_tup_hot_upd`/`n_tup_upd` and `n_dead_tup` in `pg_stat_user_tables` for these tables.
- **Reservation partitions (0035).** Every admission reads its workspace's pending reservations whose lease expired through the partial index `governance_leases` (`lease_expires_at` where `state='pending'`). Each settlement leaves a dead entry in that index, and the read walks every dead entry whose lease time has passed until vacuum removes it. With PostgreSQL's defaults a month partition is vacuumed only after 20 % of its rows are dead, and a vacuum that finds dead rows on fewer than 2 % of the pages skips index cleanup entirely, so on a large partition the dead entries pile up for a long time. `0035_counter_autovacuum.sql` therefore sets, on every `governance_reservations` partition (and `omg_ensure_partitions` on every month it creates): `autovacuum_vacuum_scale_factor=0.01`, `autovacuum_vacuum_threshold=10000`, `vacuum_index_cleanup=on`, `autovacuum_vacuum_cost_limit=2000`, `autovacuum_vacuum_cost_delay=2`. Each vacuum then scans the partition's seven indexes in full, which is sequential I/O proportional to the month's size; at very high sustained rates on a large month you may raise the threshold. Check with `SELECT relname,n_dead_tup,last_autovacuum,autovacuum_count FROM pg_stat_user_tables WHERE relname LIKE 'governance_reservations%'`. Measured (project `omg-loadtest-fu`, 2 M attempts in one 2.0 M-row partition, one replica, 45 s runs; the same image, with the 0034 defaults restored for "before"): after four 1,000/s runs the "before" partition carried 280 k dead rows (no autovacuum yet) and the admission `limits` statement averaged 0.38–0.48 ms; with 0035 it stayed at 0.14–0.15 ms (7 autovacuums, 7.7 k dead rows at the end). Saturation at 1,500/s offered: 989.4 → 1,031.7 ok/s (+4.3 %); the following 1,000/s run: 951.9 → 995.6 ok/s (+4.6 %, p99 170 → 149 ms). Budget verify and the ledger invariants passed in every run. No index change was needed.
- **Recommended server settings.** Per-table parameters only work if autovacuum has workers and budget to run them. Starting points for a dedicated primary: `autovacuum_max_workers=5` (hot counter tables, the current reservation, execution and ledger partitions and `admission_lock_waits` can need vacuum at the same time; PostgreSQL 17's default is 3), `autovacuum_naptime=15s` (default 1 min: how soon a table over its threshold is noticed), `autovacuum_vacuum_cost_limit=2000` (default -1 = `vacuum_cost_limit` 200; the per-table settings above override it for their tables), `autovacuum_work_mem=256MB` or more (one index pass per vacuum instead of several), `maintenance_work_mem=512MB`–`1GB` for manual `VACUUM` and index builds, and the defaults for `autovacuum_vacuum_cost_delay` (2 ms) and freezing (see [MultiXacts](#multixacts-and-history-parent-checks)). Keep long transactions off the primary: vacuum cannot remove rows a running snapshot may still see. Watch `n_dead_tup` and `last_autovacuum` for `budget_totals`, `rate_minute_counters`, `inflight_counters` and the current month's `governance_reservations_p*`/`inference_executions_p*` partitions.
- **Minute counters** older than ten minutes are pruned every minute by `serve` (the `maintenance` lease holder; concurrent pruners would skip each other's rows anyway).
- **Group commit.** Keep `synchronous_commit=on`: every upstream attempt needs a durable reservation. `commit_delay` (with `commit_siblings`) lets concurrent commits share one WAL flush, but only helps when at least `commit_siblings` transactions commit at once. Under the installation lock that never happened. With scoped admission many transactions commit at once; see the [P3 results](#scoped-admission-p3) for the measured effect before changing the defaults (`commit_delay=0`, `commit_siblings=5`). On storage with slow flushes (network disks, synchronous replicas) start at `commit_delay=200`–`1000` µs with `commit_siblings=5` and keep it only if commit latency p99 and throughput improve. The load-test stack exposes both as `OMG_LOADTEST_COMMIT_DELAY` and `OMG_LOADTEST_COMMIT_SIBLINGS`.

## PgBouncer

Use transaction pooling with PgBouncer **1.21 or later and `max_prepared_statements` > 0** (200 in the load-test stack). sqlx caches *named* prepared statements per connection; without protocol-level prepared-statement support, statements land on server connections that never prepared them, and the gateway fails closed (see finding 6 above). With support enabled, PgBouncer re-prepares transparently; the baseline runs saw a few hundred server-side parses for tens of thousands of transactions.

- Put the server's SCRAM secrets in `auth_file` (copy `rolpassword` from `pg_authid`), or use `auth_query`. With plaintext passwords in the userlist, PgBouncer authenticated clients against a secret with its own salt and then intermittently failed the server login ("password authentication failed"). `scripts/loadtest.py up` writes the userlist from the server's secrets.
- Add `ignore_startup_parameters = extra_float_digits`; sqlx sends it at connect time.
- Run `migrate` directly against PostgreSQL, never through PgBouncer. It takes a session-level advisory lock.
- LISTEN does not work through transaction pooling: set `GATEWAY_LISTEN_DATABASE_URL` to PostgreSQL directly (one extra session connection per replica; count it in `max_connections`) or to a session-mode pooler database. `NOTIFY` from triggers works through any pool. Without a working listener the caches still follow changes through the 1 s version poll.
- Everything the gateway does inside transactions works in transaction mode: transaction-scoped advisory locks, `SET LOCAL`, and `SET TRANSACTION ISOLATION LEVEL`.
- Size `default_pool_size` for the sum of replica pools that are actually busy, not their maximum. With scoped admission, busy server connections ≈ (admissions + settlements per second) × mean transaction time; each replica admits on at most half its pool and settles on three tenths of it.

## Finding slow queries and lock waits

Enable `pg_stat_statements` (and I/O timing) on the server. This needs a restart:

```
shared_preload_libraries = 'pg_stat_statements'
pg_stat_statements.track = all     # include statements inside the budget-totals triggers
track_io_timing = on
```

Create the extension in the `postgres` maintenance database, **not** in the gateway database. Readiness refuses unknown relations in the gateway database's `public` schema, and the extension's view is one. The statistics cover every database anyway. Read them as a role with `pg_read_all_stats`:

```sql
-- In the postgres database: CREATE EXTENSION IF NOT EXISTS pg_stat_statements;
-- Reset before a measurement window:
SELECT pg_stat_statements_reset();
-- Top statements by total time for the gateway database:
SELECT left(regexp_replace(query, '\s+', ' ', 'g'), 120) AS query, calls,
       round(total_exec_time::numeric, 1) AS total_ms, round(mean_exec_time::numeric, 3) AS mean_ms,
       rows, toplevel, shared_blks_read, round(shared_blk_read_time::numeric, 1) AS read_ms
FROM pg_stat_statements
WHERE dbid = (SELECT oid FROM pg_database WHERE datname = 'gateway')
ORDER BY total_exec_time DESC LIMIT 20;
```

What to look for in the gateway's statements:

- `SELECT omg_lock_scope_rows(…)`: scoped admission and settlement locking their workspace's totals and counter rows; its time is mostly **waiting** for other transactions of the same workspace (a hot workspace).
- `SELECT lineage,kind FROM omg_admission_locks(…)`: the shared catalog and authority locks; it waits only behind a management change of the same scope or an exclusive catalog change.
- `SELECT id FROM installation WHERE singleton FOR NO KEY UPDATE`: global admission mode and management only; its total time is time spent **waiting** for the installation lock. When it dominates in the global mode, the database is idle behind one serializer.
- `WITH accounting AS (…)`: per-minute rate accounting by scan. Since 0026 admission never runs it (the installation layer, its last user, was removed); workspace and key layers read `rate_minute_counters`/`inflight_counters` inside the single `WITH pol AS MATERIALIZED …` limits statement.
- `INSERT INTO rate_minute_counters …`/`inflight_counters …` (non-top-level): rate-counter trigger fan-out, two scopes per write.
- `INSERT INTO budget_totals …` (non-top-level): trigger fan-out, workspace and key scopes (8 rows: 2 scopes × 4 periods) per reservation or execution write; no installation row since 0026.
- A `budget verify` full scan, if one ran in the window.

Lock waits, live, in the gateway database (any role that can see `pg_stat_activity`, such as `pg_monitor`):

```sql
-- Who waits for whom right now:
SELECT waiting.pid, waiting.wait_event_type, waiting.wait_event,
       round(extract(epoch FROM clock_timestamp() - waiting.query_start)::numeric * 1000, 1) AS waiting_ms,
       pg_blocking_pids(waiting.pid) AS blocked_by,
       left(regexp_replace(waiting.query, '\s+', ' ', 'g'), 80) AS query
FROM pg_stat_activity waiting
WHERE waiting.datname = current_database() AND cardinality(pg_blocking_pids(waiting.pid)) > 0
ORDER BY waiting.query_start;
-- Wait events of active sessions, and ungranted locks by type:
SELECT wait_event_type, wait_event, count(*) FROM pg_stat_activity
WHERE datname = current_database() AND state <> 'idle' GROUP BY 1, 2 ORDER BY 3 DESC;
SELECT locktype, mode, count(*) FROM pg_locks l JOIN pg_database d ON d.oid = l.database
WHERE d.datname = current_database() AND NOT l.granted GROUP BY 1, 2;
```

In the global mode, overload shows a convoy on the installation row: one session waiting on `transactionid` and the rest on `tuple` `ExclusiveLock`, each blocked by the sessions ahead of it. With scoped admission, waits are per workspace (`tuple`/`transactionid` on `budget_totals` rows) or, briefly, `advisory` behind a management change; a deadlock is reported in the server log and counted in `gateway_lock_deadlock_retries_total`. For a history of waits, set `log_lock_waits = on` (with `deadlock_timeout`, default 1 s, as the threshold) and read the server log. Use the gateway's `gateway_admission_seconds{phase="locks"}` for the client-side view.

## Budget totals verification

`budget_totals` (migration 0015) holds settled spend, active holds and unresolved counters per scope (workspace, key lineage; the installation scope was removed in 0026), period (day, ISO week, month, lifetime) and UTC period start, in exact integer micro-USD. Admission reads it instead of scanning history. Check it against a full scan of reservations and executions:

```sh
open-model-gateway budget verify
```

- **What it does:** it runs read-only in one `REPEATABLE READ` snapshot and takes no installation lock. Traffic can continue, but the scan reads every reservation, so run it off-peak on large installations. It prints JSON (`buckets`, `mismatch_count`, up to 20 example `mismatches`) and exits nonzero on any difference. It works with the runtime role's grants.
- **What it compares:** every budget-total column, including the 0025 unknown-cost split (`held_unknown_microusd`, `unresolved_unknown`), and the 0024 rate counters: every in-flight counter against all pending reservations, and every minute counter of the last five minutes against that window's reservations and executions (`rate_buckets`, `rate_mismatch_count`, `rate_mismatches`). Older minute rows are not read by admission and are pruned by maintenance, so they are not compared.
- **Partitions and archives:** the scan reads every partition through the parents and adds the recorded contributions of archived months (`archived_budget_contributions`), so it stays exact after `archive partition`.
- **When to run it:** after `migrate` (the migration backfills from existing history), after restores (restore drill step 5), after archiving a month, and periodically.
- **If it reports a mismatch:** do not edit, delete or "fix" reservations or the ledger. Drift is only possible through manual owner-level edits, such as deleting history, re-keying lineages, or disabling triggers. The runtime role cannot update keys, delete rows or truncate the table. Preserve the report and escalate. The table can be rebuilt by the migrator from the same scan the migration uses.
