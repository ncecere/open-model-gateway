-- Installation-wide settings (Admin › Settings). One row, seeded here; runtime may
-- update reviewed columns only (no INSERT/DELETE). The display name stays in
-- installation.name. SMTP passwords are never stored: only an allowlisted
-- `env:NAME` reference. Prompt/response bodies are never stored at all.
CREATE TABLE installation_settings(
 singleton boolean PRIMARY KEY DEFAULT true CHECK(singleton) REFERENCES installation(singleton),
 support_url text CHECK(support_url IS NULL OR (length(support_url) BETWEEN 9 AND 2048 AND support_url LIKE 'https://%')),
 logo_url text CHECK(logo_url IS NULL OR (length(logo_url) BETWEEN 9 AND 2048 AND logo_url LIKE 'https://%')),
 human_key_max_lifetime_days integer NOT NULL DEFAULT 365 CHECK(human_key_max_lifetime_days BETWEEN 1 AND 365),
 openrouter_data_collection text NOT NULL DEFAULT 'deny' CHECK(openrouter_data_collection IN ('deny','allow')),
 request_log_retention_days integer CHECK(request_log_retention_days IS NULL OR request_log_retention_days BETWEEN 30 AND 3650),
 smtp_host text CHECK(smtp_host IS NULL OR length(smtp_host) BETWEEN 1 AND 253),
 smtp_port integer CHECK(smtp_port IS NULL OR smtp_port BETWEEN 1 AND 65535),
 smtp_tls text CHECK(smtp_tls IS NULL OR smtp_tls IN ('starttls','implicit','none')),
 smtp_username text CHECK(smtp_username IS NULL OR length(smtp_username) BETWEEN 1 AND 256),
 smtp_password_ref text CHECK(smtp_password_ref IS NULL OR smtp_password_ref ~ '^env:[A-Z_][A-Z0-9_]{0,127}$'),
 smtp_from_address text CHECK(smtp_from_address IS NULL OR length(smtp_from_address) BETWEEN 3 AND 320),
 smtp_from_name text CHECK(smtp_from_name IS NULL OR length(smtp_from_name) BETWEEN 1 AND 120),
 smtp_last_test_at timestamptz,
 smtp_last_test_ok boolean,
 smtp_last_test_error text CHECK(smtp_last_test_error IS NULL OR smtp_last_test_error IN ('credential','address','connection','tls','authentication','rejected','timeout')),
 updated_at timestamptz NOT NULL DEFAULT now(),
 updated_by uuid REFERENCES users(id),
 -- Delivery is either fully configured or entirely absent.
 CHECK((smtp_host IS NULL) = (smtp_port IS NULL) AND (smtp_host IS NULL) = (smtp_tls IS NULL) AND (smtp_host IS NULL) = (smtp_from_address IS NULL)),
 CHECK(smtp_host IS NOT NULL OR (smtp_username IS NULL AND smtp_password_ref IS NULL AND smtp_from_name IS NULL)),
 CHECK((smtp_username IS NULL) = (smtp_password_ref IS NULL)),
 CHECK((smtp_last_test_at IS NULL) = (smtp_last_test_ok IS NULL) AND (smtp_last_test_error IS NULL OR smtp_last_test_ok = false))
);
INSERT INTO installation_settings(singleton) VALUES(true);
