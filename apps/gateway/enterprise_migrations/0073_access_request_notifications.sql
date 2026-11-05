-- #83 slice B3: per-user read state of access-request notifications. The
-- notifications themselves are derived live from access_requests on every
-- read (visibility is rechecked there); only "I have seen this request in
-- this state" is stored, so a decision makes it unread again for the
-- requester. In-app only: nothing here feeds alert delivery.
CREATE TABLE access_request_notification_reads (
 user_id uuid NOT NULL REFERENCES users(id),
 request_id uuid NOT NULL REFERENCES access_requests(id),
 state text NOT NULL CHECK (state IN ('pending','approved','denied','expired')),
 read_at timestamptz NOT NULL DEFAULT clock_timestamp(),
 PRIMARY KEY (user_id, request_id, state)
);
CREATE TRIGGER access_request_notification_reads_immutable BEFORE UPDATE OR DELETE ON access_request_notification_reads FOR EACH ROW EXECUTE FUNCTION immutable_history();
CREATE TRIGGER access_request_notification_reads_no_truncate BEFORE TRUNCATE ON access_request_notification_reads FOR EACH STATEMENT EXECUTE FUNCTION immutable_history();
-- The approver feed walks the viewer's owner/admin workspaces to their
-- pending requests; the requester feed reads decided ones per requester.
CREATE INDEX access_requests_pending_workspace ON access_requests(workspace_id,created_at DESC) WHERE status='pending';
CREATE INDEX access_requests_decided_requester ON access_requests(requester,decided_at DESC) WHERE status IN ('approved','denied','expired');
