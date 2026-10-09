-- SCIM 2.0 provisioning (0014). Explicit operator upgrade only (`migrate`).
--
-- A SCIM User resource is a gateway user (resource id = users.id). scim_users holds the
-- identity provider's directory attributes; `active` is the provider's view, separate from
-- administrative suspension. Users are never deleted by SCIM: deactivation suspends them
-- and the normal 30-day cleanup tombstones them (clearing these attributes).
-- SCIM provisioning grants no access by itself: platform roles and workspace membership
-- still come from manual grants or issuer group mappings (group provenance).
CREATE TABLE scim_users(
 user_id uuid PRIMARY KEY REFERENCES users(id),
 user_name text CHECK(user_name IS NULL OR (length(user_name) BETWEEN 1 AND 320 AND user_name !~ '[[:cntrl:]]')),
 external_id text CHECK(external_id IS NULL OR (length(external_id) BETWEEN 1 AND 512 AND external_id !~ '[[:cntrl:]]')),
 given_name text CHECK(given_name IS NULL OR (length(given_name) BETWEEN 1 AND 200 AND given_name !~ '[[:cntrl:]]')),
 family_name text CHECK(family_name IS NULL OR (length(family_name) BETWEEN 1 AND 200 AND family_name !~ '[[:cntrl:]]')),
 active boolean NOT NULL DEFAULT true,
 created_at timestamptz NOT NULL DEFAULT now(),
 updated_at timestamptz NOT NULL DEFAULT now()
);
CREATE UNIQUE INDEX scim_users_user_name ON scim_users(lower(user_name)) WHERE user_name IS NOT NULL;
CREATE INDEX scim_users_external_id ON scim_users(external_id) WHERE external_id IS NOT NULL;
-- A pushed group's display name and external id are matched against
-- oidc_group_mappings.group_value for the configured issuer.
CREATE TABLE scim_groups(
 id uuid PRIMARY KEY,
 display_name text NOT NULL CHECK(length(display_name) BETWEEN 1 AND 512 AND display_name !~ '[[:cntrl:]]'),
 external_id text CHECK(external_id IS NULL OR (length(external_id) BETWEEN 1 AND 512 AND external_id !~ '[[:cntrl:]]')),
 created_at timestamptz NOT NULL DEFAULT now(),
 updated_at timestamptz NOT NULL DEFAULT now()
);
CREATE UNIQUE INDEX scim_groups_display_name ON scim_groups(display_name);
CREATE TABLE scim_group_members(
 group_id uuid NOT NULL REFERENCES scim_groups(id) ON DELETE CASCADE,
 user_id uuid NOT NULL REFERENCES users(id),
 added_at timestamptz NOT NULL DEFAULT now(),
 PRIMARY KEY(group_id,user_id)
);
CREATE INDEX scim_group_members_user ON scim_group_members(user_id);
-- Last successful SCIM write (Admin › Settings › Sign-in). One seeded row.
CREATE TABLE scim_state(
 singleton boolean PRIMARY KEY DEFAULT true CHECK(singleton) REFERENCES installation(singleton),
 last_write_at timestamptz
);
INSERT INTO scim_state(singleton) VALUES(true);
