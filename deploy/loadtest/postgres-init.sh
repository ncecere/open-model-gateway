#!/bin/sh
# Runs once for a NEW load-test data volume. Creates the migrator (schema
# owner) and runtime roles with the generated passwords, and enables
# pg_stat_statements in the `postgres` maintenance database only, so no
# extension objects appear in the gateway database (readiness preflight
# rejects unknown relations in its public schema).
set +x
set -eu
psql --no-psqlrc --set=ON_ERROR_STOP=1 --username "$POSTGRES_USER" --dbname postgres \
  --set=migrator_password="$MIGRATOR_PASSWORD" --set=runtime_password="$RUNTIME_PASSWORD" <<'SQL'
CREATE ROLE gateway_migrator LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION PASSWORD :'migrator_password';
CREATE ROLE gateway_runtime LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION PASSWORD :'runtime_password';
CREATE EXTENSION IF NOT EXISTS pg_stat_statements;
GRANT pg_read_all_stats TO gateway_migrator;
SQL
