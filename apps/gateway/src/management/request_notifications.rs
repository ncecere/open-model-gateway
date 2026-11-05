//! #83 slice B3: access-request events in the in-app notification feed.
//!
//! Derived live from `access_requests` on every read, so visibility is the
//! current authority, never a copy made when the request changed:
//! - approvers: pending, unexpired requests of an enabled Team/Project where
//!   the viewer is a live owner/admin, and pending membership requests for a
//!   Platform Admin (who decides those on the platform path); never the
//!   requester's own request;
//! - requesters: their own requests decided (approved, denied, expired) in
//!   the last 30 days.
//!
//! Only per-user read state is stored (`access_request_notification_reads`,
//! keyed by request and state, so a decision is unread again for the
//! requester). Nothing here feeds alert delivery (email or webhooks).

/// Rows `(at, id, item, is_read)` visible to the user bound as `user`, with
/// the viewer's live Platform Admin flag bound as `admin`.
pub(super) fn items(user: &str, admin: &str) -> String {
    let item = |audience: &str, state: &str, at: &str| {
        format!(
            "jsonb_build_object('source','access_request','id',r.id,'audience','{audience}','state',{state},'target_kind',r.target_kind,'workspace_id',r.workspace_id,'workspace_name_at_request',r.workspace_name_at_request,'model_name_at_request',r.model_name_at_request,'at',{at},'read',x.request_id IS NOT NULL)"
        )
    };
    let approver = item("approver", "'pending'", "r.created_at");
    let requester = item("requester", "r.status", "r.decided_at");
    format!(
        "SELECT r.created_at AS at,r.id,{approver} AS item,x.request_id IS NOT NULL AS is_read,'pending'::text AS state \
         FROM access_requests r JOIN workspaces w ON w.id=r.workspace_id \
         LEFT JOIN access_request_notification_reads x ON x.request_id=r.id AND x.user_id={user} AND x.state='pending' \
         WHERE r.status='pending' AND r.expires_at>clock_timestamp() AND r.requester<>{user} \
         AND w.kind IN ('team','project') AND w.disabled_at IS NULL \
         AND (EXISTS(SELECT 1 FROM effective_workspace_memberships m WHERE m.workspace_id=r.workspace_id AND m.user_id={user} AND m.role IN ('owner','admin')) \
              OR (r.target_kind='workspace_membership' AND {admin})) \
         UNION ALL \
         SELECT r.decided_at,r.id,{requester},x.request_id IS NOT NULL,r.status \
         FROM access_requests r \
         LEFT JOIN access_request_notification_reads x ON x.request_id=r.id AND x.user_id={user} AND x.state=r.status \
         WHERE r.requester={user} AND r.status IN ('approved','denied','expired') AND r.decided_at>clock_timestamp()-interval '30 days'"
    )
}

/// Mark the visible, unread request notifications read: all of them, or
/// those among `ids` (other ids are ignored, never disclosed).
pub(super) fn mark_read(user: &str, admin: &str, ids: &str) -> String {
    let items = items(user, admin);
    format!(
        "INSERT INTO access_request_notification_reads(user_id,request_id,state) \
         SELECT {user},n.id,n.state FROM ({items}) n WHERE NOT n.is_read AND ({ids}::uuid[] IS NULL OR n.id=ANY({ids})) \
         ON CONFLICT DO NOTHING"
    )
}
