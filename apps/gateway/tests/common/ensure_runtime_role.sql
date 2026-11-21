-- Roles live in the cluster catalog, even when fixtures use different databases.
DO $$
BEGIN
    IF NOT EXISTS (SELECT FROM pg_roles WHERE rolname = 'gateway_runtime') THEN
        CREATE ROLE gateway_runtime NOLOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION NOBYPASSRLS;
    END IF;
EXCEPTION WHEN duplicate_object OR unique_violation THEN
    -- Only a peer that created the expected role satisfies this fixture.
    IF NOT EXISTS (SELECT FROM pg_roles WHERE rolname = 'gateway_runtime') THEN
        RAISE;
    END IF;
END
$$;
