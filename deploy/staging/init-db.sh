#!/bin/sh
set +x
set -eu
# Runs only for a NEW, dedicated PostgreSQL data volume. Secrets never appear
# in argv or stdout. psql quotes them as SQL literals via \getenv variables.
MIGRATOR_PASSWORD=$(cat /run/secrets/migrator_password)
RUNTIME_PASSWORD=$(cat /run/secrets/runtime_password)
export MIGRATOR_PASSWORD RUNTIME_PASSWORD
psql --no-psqlrc --set=ON_ERROR_STOP=1 --username "$POSTGRES_USER" --dbname "$POSTGRES_DB" --file=/opt/gateway/init-db.sql
unset MIGRATOR_PASSWORD RUNTIME_PASSWORD
