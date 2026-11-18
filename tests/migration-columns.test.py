"""Check explicit PostgreSQL column declarations for names that need secret-storage review."""
from collections import Counter
from pathlib import Path
import re
import tempfile
import unittest

MIGRATIONS = Path(__file__).resolve().parents[1] / "apps/gateway/enterprise_migrations"
SENSITIVE = re.compile("secret|token|password|credential", re.I)


# Match quoted tokens before punctuation so embedded commas and keywords stay opaque.
LEXEME = re.compile(r"""\s+|--[^\n]*|/\*|\$(?:[a-zA-Z_]\w*)?\$|[eE]'(?:\\.|''|[^'\\])*'|'(?:''|[^'])*'|"(?:""|[^"])*"|[^\W\d][\w$]*|.""", re.S)
BLOCK = re.compile(r"/\*|\*/")


def sql_tokens(sql):
    pos = 0
    previous = None
    while pos < len(sql):
        match = LEXEME.match(sql, pos)
        word = match[0]
        pos = match.end()
        if word.isspace() or word.startswith("--"):
            continue
        if word == "/*":
            depth = 1
            for marker in BLOCK.finditer(sql, pos):
                depth += 1 if marker[0] == "/*" else -1
                if not depth:
                    pos = marker.end()
                    break
            else:
                raise ValueError("unterminated block comment")
            continue
        if word.startswith("$") and word.endswith("$") and len(word) >= 2:
            end = sql.index(word, pos)
            # Migrations also contain literal DDL inside DO and function bodies.
            if previous in ("do", "as"):
                yield ";"
                yield from sql_tokens(sql[pos:end])
                yield ";"
            else:
                yield "'literal'"
            pos = end + len(word)
        elif word.startswith('"'):
            yield word
        else:
            yield word.lower()
        previous = word.lower()


# Each item starts at the first table element or ALTER action, or after a
# comma outside expressions, type modifiers, subscripts and nested queries.
def items(words, start):
    depth = 0
    yield start
    for i in range(start, len(words)):
        word = words[i]
        if word == ";" or (word == ")" and depth == 0):
            break
        if word in ("(", "["):
            depth += 1
        elif word in (")", "]"):
            depth -= 1
        elif word == "," and depth == 0:
            yield i + 1


def columns(sql):
    # A terminator for lookahead also handles a final statement without a semicolon.
    words = list(sql_tokens(sql)) + [";"] * 3
    for start, verb in enumerate(words):
        if verb not in ("create", "alter"):
            continue
        i = start + 1
        while words[i] in ("global", "local", "temporary", "temp", "unlogged"):
            i += 1
        if words[i] != "table":
            continue
        i += 1
        while words[i] in ("if", "not", "exists", "only"):
            i += 1
        table = words[i]
        i += 1
        while words[i] == ".":
            table += "." + words[i + 1]
            i += 2
        if verb == "create":
            if words[i] != "(":
                continue
            i += 1
        elif words[i] == "*":
            i += 1
        for j in items(words, i):
            if verb == "alter":
                if words[j] == "rename":
                    j += 1 + (words[j + 1] == "column")
                    if words[j + 1] == "to" and SENSITIVE.search(words[j + 2]):
                        yield table, words[j + 2], "(rename)"
                    continue
                if words[j] != "add":
                    continue
                j += 1 + (words[j + 1] == "column")
                if words[j:j + 3] == ["if", "not", "exists"]:
                    j += 3
            if SENSITIVE.search(words[j]):
                yield table, words[j], words[j + 1]


# Each exception identifies one declaration in one migration and its leading type name.
REVIEWED = (
    # Lookup digests for inference keys, browser sessions, invitations and admin tokens.
    ("0001_enterprise.sql", "api_keys", "secret_hash", "bytea"),
    ("0001_enterprise.sql", "browser_sessions", "token_hash", "bytea"),
    ("0001_enterprise.sql", "workspace_invitations", "token_hash", "bytea"),
    ("0051_admin_api_tokens.sql", "admin_api_tokens", "secret_hash", "bytea"),
    # Configuration references for provider authentication, SMTP or webhook signing.
    ("0001_enterprise.sql", "provider_connections", "credential_ref", "text"),
    ("0010_installation_settings.sql", "installation_settings", "smtp_password_ref", "text"),
    ("0057_alert_webhooks.sql", "alert_webhook_destinations", "current_secret_ref", "text"),
    ("0057_alert_webhooks.sql", "alert_webhook_destinations", "previous_secret_ref", "text"),
    # Model token quantities, rate limits and usage aggregation counts.
    ("0001_enterprise.sql", "deployment_prices", "input_token_limit", "bigint"),
    ("0001_enterprise.sql", "deployment_prices", "output_token_limit", "bigint"),
    ("0001_enterprise.sql", "governance_reservations", "input_tokens", "bigint"),
    ("0001_enterprise.sql", "governance_reservations", "output_tokens", "bigint"),
    ("0001_enterprise.sql", "governance_reservations", "reserved_tokens", "bigint"),
    ("0001_enterprise.sql", "inference_executions", "input_tokens", "bigint"),
    ("0001_enterprise.sql", "inference_executions", "output_tokens", "bigint"),
    ("0001_enterprise.sql", "installation_policy", "tokens_per_minute", "bigint"),
    ("0001_enterprise.sql", "key_policies", "tokens_per_minute", "bigint"),
    ("0001_enterprise.sql", "monetary_ledger", "input_tokens", "bigint"),
    ("0001_enterprise.sql", "monetary_ledger", "output_tokens", "bigint"),
    ("0001_enterprise.sql", "workspace_local_policies", "tokens_per_minute", "bigint"),
    ("0001_enterprise.sql", "workspace_platform_policy_overrides", "tokens_per_minute", "bigint"),
    ("0001_enterprise.sql", "workspace_type_policies", "tokens_per_minute", "bigint"),
    ("0009_request_telemetry.sql", "inference_executions", "reasoning_tokens", "bigint"),
    ("0016_async_jobs.sql", "async_job_files", "output_token_sum", "bigint"),
    ("0017_realtime.sql", "realtime_responses", "cached_audio_tokens", "bigint"),
    ("0017_realtime.sql", "realtime_responses", "cached_text_tokens", "bigint"),
    ("0017_realtime.sql", "realtime_responses", "input_audio_tokens", "bigint"),
    ("0017_realtime.sql", "realtime_responses", "input_text_tokens", "bigint"),
    ("0017_realtime.sql", "realtime_responses", "output_audio_tokens", "bigint"),
    ("0017_realtime.sql", "realtime_responses", "output_text_tokens", "bigint"),
    ("0024_rate_counters.sql", "rate_minute_counters", "tokens", "numeric"),
    ("0032_usage_rollups.sql", "usage_rollups_hourly", "cache_input_tokens", "numeric"),
    ("0032_usage_rollups.sql", "usage_rollups_hourly", "cache_read_tokens", "numeric"),
    ("0032_usage_rollups.sql", "usage_rollups_hourly", "input_tokens", "numeric"),
    ("0032_usage_rollups.sql", "usage_rollups_hourly", "output_tokens", "numeric"),
    ("0032_usage_rollups.sql", "usage_rollups_hourly", "priced_tokens", "numeric"),
    ("0032_usage_rollups.sql", "usage_rollups_hourly", "tokens", "numeric"),
    ("0032_usage_rollups.sql", "usage_rollups_hourly", "unknown_token_attempts", "bigint"),
    # Time to first generated model token, measured in milliseconds.
    ("0009_request_telemetry.sql", "inference_executions", "time_to_first_token_ms", "bigint"),
    # Internal claim and outbox lease identifiers used to fence worker writes.
    ("0050_alert_recipient_outbox.sql", "alert_recipient_outbox", "lease_token", "uuid"),
    ("0057_alert_webhooks.sql", "alert_webhook_outbox", "lease_token", "uuid"),
    ("0063_report_generation.sql", "report_generation_claims", "token", "uuid"),
    ("0063_report_generation.sql", "report_generation_outcomes", "claim_token", "uuid"),
    ("0063_report_generation.sql", "report_runs", "claim_token", "uuid"),
    # Foreign keys to admin_api_tokens.id for audit attribution.
    ("0052_admin_token_audit.sql", "admin_token_attempts", "token_id", "uuid"),
    ("0052_admin_token_audit.sql", "audit_events", "admin_token_id", "uuid"),
    # Expiry of the previous webhook signing reference during rotation.
    ("0057_alert_webhooks.sql", "alert_webhook_destinations", "previous_secret_until", "timestamptz"),
)


def problems(directory, reviewed=REVIEWED):
    files = sorted(directory.glob("*.sql"))
    if not files:
        raise ValueError(f"no migration SQL in {directory}")
    found = Counter((file.name, *column) for file in files
                    for column in columns(file.read_text(encoding="utf-8")))
    expected = Counter(reviewed)
    return [f"{label}: {' '.join(key)}" for label, difference in (
        ("unreviewed", found - expected), ("stale", expected - found)
    ) for key in sorted(difference.elements())]


class MigrationColumns(unittest.TestCase):
    def test_repository(self):
        findings = problems(MIGRATIONS)
        self.assertFalse(findings, "\n".join(findings))

    def test_planted_columns(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            migration = root / "9000_fixture.sql"
            migration.write_text("""
                CREATE TABLE IF NOT EXISTS public.fixture (
                    id int CHECK(id IN (1,2)), "ExamplePassword" text,
                    note text DEFAULT 'token);', CONSTRAINT password_check CHECK(id > 0)
                );
                ALTER TABLE ONLY public.fixture ALTER id SET DEFAULT 1,
                    ADD /* column */ COLUMN IF NOT EXISTS example_token_ref text;
                ALTER TABLE public.fixture RENAME note TO upstream_credential;
                DO $body$ BEGIN
                    ALTER TABLE public.fixture ADD signing_secret text;
                END $body$;
            """)
            self.assertEqual(problems(root, ()), [
                'unreviewed: 9000_fixture.sql public.fixture "ExamplePassword" text',
                'unreviewed: 9000_fixture.sql public.fixture example_token_ref text',
                'unreviewed: 9000_fixture.sql public.fixture signing_secret text',
                'unreviewed: 9000_fixture.sql public.fixture upstream_credential (rename)',
            ])
            migration.write_text("CREATE TABLE public.fixture (note text DEFAULT 'password');")
            self.assertEqual(problems(root, ()), [])
            stale = [(migration.name, "public.fixture", "example_token_ref", "text")]
            self.assertEqual(problems(root, stale), [
                "stale: 9000_fixture.sql public.fixture example_token_ref text",
            ])


if __name__ == "__main__":
    unittest.main()
