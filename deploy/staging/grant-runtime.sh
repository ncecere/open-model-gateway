#!/bin/sh
set +x
set -eu
PGPASSWORD=$(cat /run/secrets/migrator_password)
export PGPASSWORD
exec psql --no-psqlrc --set=ON_ERROR_STOP=1 --file=/opt/gateway/runtime-grants.sql
