-- #90 slice 4: per-user read state of access review reminders. A reminder
-- (one per open campaign in which the viewer has undecided items they may
-- decide) is derived live on every read; only "I have seen this campaign's
-- reminder in this state" is stored, so it is unread again when the campaign
-- becomes due soon or overdue. In-app only: nothing here feeds alert
-- delivery. Insert-only, server read time.
CREATE TABLE access_review_notification_reads (
 user_id uuid NOT NULL REFERENCES users(id),
 campaign_id uuid NOT NULL REFERENCES access_review_campaigns(id),
 state text NOT NULL CHECK (state IN ('open','due_soon','overdue')),
 read_at timestamptz NOT NULL DEFAULT clock_timestamp(),
 PRIMARY KEY (user_id, campaign_id, state)
);
CREATE TRIGGER access_review_notification_reads_immutable BEFORE UPDATE OR DELETE ON access_review_notification_reads FOR EACH ROW EXECUTE FUNCTION immutable_history();
CREATE TRIGGER access_review_notification_reads_no_truncate BEFORE TRUNCATE ON access_review_notification_reads FOR EACH STATEMENT EXECUTE FUNCTION immutable_history();
-- Undecided items (no decision row) are found per campaign through the items
-- index and the decisions' (item_id, seq) unique index.
