//! Legal holds (#75): while a hold is active on a user or on a team or
//! project workspace, the gateway removes none of that scope's data.
//!
//! Each removal path asks the same questions, as SQL fragments here, inside
//! its own transaction:
//! - departed-user cleanup (`lifecycle.rs`): is the user held? Checked again
//!   after the user's row lock, which placing a hold also takes, so the two
//!   serialize;
//! - request-log compaction (`maintenance.rs`): is the execution's workspace
//!   held, or the user its key was issued to, or a personal workspace's owner?
//! - stored-file expiry (`filestore/sweep.rs`): the same for the file's
//!   workspace and creator (an unfinished upload is still removed);
//! - `archive partition` (`archive.rs`): does the month hold any held data?
//!
//! A hold that commits while one of the batch jobs is running applies from
//! that job's next run. Holds grant no read access.

/// The user `column` names is held.
pub fn user_held(column: &str) -> String {
    format!(
        "EXISTS(SELECT 1 FROM legal_holds lh WHERE lh.released_at IS NULL AND lh.user_id={column})"
    )
}

/// The workspace `column` names is held, directly (team/project) or through
/// a personal workspace's owner.
pub fn workspace_held(column: &str) -> String {
    format!(
        "(EXISTS(SELECT 1 FROM legal_holds lh WHERE lh.released_at IS NULL AND lh.workspace_id={column}) \
         OR EXISTS(SELECT 1 FROM workspaces lw JOIN legal_holds lh ON lh.user_id=lw.owner_user_id AND lh.released_at IS NULL \
                   WHERE lw.id={column} AND lw.kind='personal'))"
    )
}

/// An execution (alias `alias`) is held: its workspace, or the user its key
/// was issued to.
pub fn execution_held(alias: &str) -> String {
    format!(
        "({} OR EXISTS(SELECT 1 FROM api_keys lk JOIN legal_holds lh ON lh.user_id=lk.issued_to_user_id AND lh.released_at IS NULL WHERE lk.id={alias}.api_key_id))",
        workspace_held(&format!("{alias}.workspace_id"))
    )
}

/// A stored file (alias `alias`) is held: its workspace or its creator.
pub fn file_held(alias: &str) -> String {
    format!(
        "({} OR {})",
        workspace_held(&format!("{alias}.workspace_id")),
        user_held(&format!("{alias}.created_by_user_id"))
    )
}

/// An audit event (alias `alias`) concerns held data: its actor, its
/// workspace, or a held user or workspace it is about.
pub fn audit_held(alias: &str) -> String {
    format!(
        "({} OR {} OR ({alias}.resource_type='user' AND {}) OR ({alias}.resource_type='workspace' AND {}))",
        user_held(&format!("{alias}.actor_user_id")),
        workspace_held(&format!("{alias}.workspace_id")),
        user_held(&format!("{alias}.resource_id")),
        workspace_held(&format!("{alias}.resource_id"))
    )
}
