-- Routing is provider-independent and opt-in. Missing rows use application defaults.
CREATE TABLE model_routing_policies (
    organization_id UUID NOT NULL,
    model_id UUID NOT NULL,
    strategy TEXT NOT NULL DEFAULT 'priority' CHECK (strategy IN ('priority', 'weighted')),
    max_attempts INTEGER NOT NULL DEFAULT 1 CHECK (max_attempts BETWEEN 1 AND 3),
    allow_ambiguous_failover BOOLEAN NOT NULL DEFAULT false,
    failure_threshold INTEGER NOT NULL DEFAULT 3 CHECK (failure_threshold >= 1),
    cooldown_seconds INTEGER NOT NULL DEFAULT 30 CHECK (cooldown_seconds BETWEEN 1 AND 3600),
    required_residency TEXT CHECK (required_residency <> 'unspecified' AND required_residency ~ '^[a-z0-9][a-z0-9._-]{0,63}$'),
    PRIMARY KEY (organization_id, model_id),
    FOREIGN KEY (organization_id, model_id) REFERENCES models(organization_id, id) ON DELETE CASCADE
);

CREATE TABLE deployment_routing (
    organization_id UUID NOT NULL,
    deployment_id UUID NOT NULL,
    priority INTEGER NOT NULL DEFAULT 0,
    weight INTEGER NOT NULL DEFAULT 1 CHECK (weight BETWEEN 1 AND 1000),
    -- An operator assertion, never inferred from provider or region. Exact matches only.
    residency TEXT NOT NULL DEFAULT 'unspecified'
        CHECK (residency ~ '^[a-z0-9][a-z0-9._-]{0,63}$'),
    operator_disabled BOOLEAN NOT NULL DEFAULT false,
    PRIMARY KEY (organization_id, deployment_id),
    FOREIGN KEY (organization_id, deployment_id) REFERENCES deployments(organization_id, id) ON DELETE CASCADE
);

-- Passive observations only: an absent row is unknown, not proof of health.
CREATE TABLE deployment_route_health (
    organization_id UUID NOT NULL,
    deployment_id UUID NOT NULL,
    consecutive_failures INTEGER NOT NULL DEFAULT 0 CHECK (consecutive_failures >= 0),
    open_until TIMESTAMPTZ,
    last_observed_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (organization_id, deployment_id),
    FOREIGN KEY (organization_id, deployment_id) REFERENCES deployments(organization_id, id) ON DELETE CASCADE
);
