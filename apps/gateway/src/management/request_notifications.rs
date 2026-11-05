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

/// Rows `(at, id, item, is_read, state)` visible to the user bound as
/// `user`, with the viewer's live Platform Admin flag bound as `admin`.
/// Approver rows start from the viewer's own owner/admin memberships (an
/// index walk per workspace, not a scan of every pending request); the
/// Platform Admin arm adds pending membership requests elsewhere.
pub(super) fn items(user: &str, admin: &str) -> String {
    let item = |audience: &str, state: &str, at: &str| {
        format!(
            "jsonb_build_object('source','access_request','id',r.id,'audience','{audience}','state',{state},'target_kind',r.target_kind,'workspace_id',r.workspace_id,'workspace_name_at_request',r.workspace_name_at_request,'model_name_at_request',r.model_name_at_request,'at',{at},'read',x.request_id IS NOT NULL)"
        )
    };
    let approver = item("approver", "'pending'", "r.created_at");
    let requester = item("requester", "r.status", "r.decided_at");
    let pending = format!(
        "r.status='pending' AND r.expires_at>clock_timestamp() AND r.requester<>{user} AND w.kind IN ('team','project') AND w.disabled_at IS NULL"
    );
    let read = |state: &str| {
        format!(
            "LEFT JOIN access_request_notification_reads x ON x.request_id=r.id AND x.user_id={user} AND x.state={state}"
        )
    };
    let pending_read = read("'pending'");
    let decided_read = read("r.status");
    let manages = format!(
        "EXISTS(SELECT 1 FROM effective_workspace_memberships m WHERE m.workspace_id=r.workspace_id AND m.user_id={user} AND m.role IN ('owner','admin'))"
    );
    format!(
        "SELECT r.created_at AS at,r.id,{approver} AS item,x.request_id IS NOT NULL AS is_read,'pending'::text AS state \
         FROM effective_workspace_memberships m JOIN access_requests r ON r.workspace_id=m.workspace_id JOIN workspaces w ON w.id=r.workspace_id {pending_read} \
         WHERE m.user_id={user} AND m.role IN ('owner','admin') AND {pending} \
         UNION ALL \
         SELECT r.created_at,r.id,{approver},x.request_id IS NOT NULL,'pending'::text \
         FROM access_requests r JOIN workspaces w ON w.id=r.workspace_id {pending_read} \
         WHERE {admin} AND r.target_kind='workspace_membership' AND {pending} AND NOT {manages} \
         UNION ALL \
         SELECT r.decided_at,r.id,{requester},x.request_id IS NOT NULL,r.status \
         FROM access_requests r {decided_read} \
         WHERE r.requester={user} AND r.status IN ('approved','denied','expired') AND r.decided_at>clock_timestamp()-interval '30 days'"
    )
}

/// Mark the visible, unread request notifications read: all of them (at
/// most 1000 per call, like incidents), or those among `ids` (other ids are
/// ignored, never disclosed). The read marks' foreign key takes a key-share
/// lock on each request, so a mark can briefly wait for a decision on it.
pub(super) fn mark_read(user: &str, admin: &str, ids: &str) -> String {
    let items = items(user, admin);
    format!(
        "INSERT INTO access_request_notification_reads(user_id,request_id,state) \
         SELECT {user},n.id,n.state FROM ({items}) n WHERE NOT n.is_read AND ({ids}::uuid[] IS NULL OR n.id=ANY({ids})) \
         ORDER BY n.at DESC LIMIT 1000 ON CONFLICT DO NOTHING"
    )
}
