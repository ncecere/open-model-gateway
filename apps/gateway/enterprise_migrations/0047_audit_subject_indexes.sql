-- #74: a person's audit entries (by actor, and about them) without scanning
-- every audit partition. Built on each partition; the gateway's own audit
-- view uses them too.
CREATE INDEX audit_events_by_actor ON audit_events(actor_user_id, created_at);
CREATE INDEX audit_events_about_user ON audit_events(resource_id, created_at) WHERE resource_type='user';
