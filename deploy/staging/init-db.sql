\getenv migrator_password MIGRATOR_PASSWORD
\getenv runtime_password RUNTIME_PASSWORD
BEGIN;
CREATE ROLE gateway_migrator LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION PASSWORD :'migrator_password';
CREATE ROLE gateway_runtime LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION PASSWORD :'runtime_password';
ALTER DATABASE gateway OWNER TO gateway_migrator;
REVOKE ALL ON DATABASE gateway, postgres, template1 FROM PUBLIC;
GRANT CONNECT ON DATABASE gateway TO gateway_migrator, gateway_runtime;
ALTER SCHEMA public OWNER TO gateway_migrator;
REVOKE ALL ON SCHEMA public FROM PUBLIC;
GRANT USAGE ON SCHEMA public TO gateway_runtime;
ALTER ROLE gateway_runtime IN DATABASE gateway SET search_path = pg_catalog, public;
ALTER DEFAULT PRIVILEGES FOR ROLE gateway_migrator REVOKE EXECUTE ON FUNCTIONS FROM PUBLIC;
COMMIT;
