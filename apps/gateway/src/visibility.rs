//! Object visibility (0038, docs/files-api.md#who-sees-which-files): which
//! files, batches (with their result files) and video jobs of a workspace a
//! caller may list, read and download, and which they may delete or cancel.
//!
//! Shared (Team/Project) workspaces carry `object_visibility`:
//!
//! | Caller | `workspace` | `member` (default) |
//! |---|---|---|
//! | Service-account key | all | all |
//! | Key of a current workspace owner/admin | all | all |
//! | Key of a regular member | all | objects of that user |
//! | Dashboard, regular member | all | own |
//! | Dashboard, workspace owner/admin | all | all |
//!
//! Personal workspaces are owner-private and unaffected. A file's owner is
//! `stored_files.created_by_user_id`; a job's owner is the user its key was
//! issued to (`api_keys.issued_to_user_id` of `async_jobs.api_key_id`).
//! Objects of service-account keys have no user owner.
//!
//! Deleting or cancelling needs more than seeing, in either mode: the
//! creating user, a service-account key or a current workspace owner/admin
//! ([`may_modify_sql`]). Others who can see the object get 403; those who
//! cannot see it get the same 404 as for a missing object.
//!
//! The rule is one SQL condition ([`visible_sql`]) evaluated inside the
//! object query itself, so the setting and the caller's live membership role
//! are read in the same snapshot as the object, lists are filtered before
//! LIMIT, and a hidden object is indistinguishable from a missing one.
use uuid::Uuid;

use crate::auth::Principal;

/// Who asks for a workspace's objects.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Viewer {
    /// A service-account key: every object of its workspace.
    Service,
    /// A key issued to this user, or this user in the dashboard.
    User(Uuid),
}

impl Viewer {
    /// The caller behind an inference key (service-account keys have no user).
    pub fn of(principal: &Principal) -> Self {
        principal.user_id.map_or(Self::Service, Self::User)
    }
    /// The bind value for [`visible_sql`]'s viewer parameter.
    pub fn user(self) -> Option<Uuid> {
        match self {
            Self::Service => None,
            Self::User(id) => Some(id),
        }
    }
}

/// SQL condition: an object of workspace `ws` owned by user `owner` is
/// visible to `viewer`. `ws` and `viewer` must be bind parameters (for
/// example `$1`); `viewer` is the [`Viewer::user`] value (NULL: a
/// service-account key). `owner` is an SQL expression over the object row
/// (NULL: no user owner). The workspace part does not depend on the row, so
/// PostgreSQL evaluates it once per statement.
pub(crate) fn visible_sql(ws: &str, viewer: &str, owner: &str) -> String {
    format!(
        "(coalesce({owner}={viewer}::uuid,false) OR coalesce((SELECT ov.kind='personal' OR ov.object_visibility='workspace' OR {viewer}::uuid IS NULL OR EXISTS(SELECT 1 FROM effective_workspace_memberships om WHERE om.workspace_id=ov.id AND om.user_id={viewer}::uuid AND om.role IN('owner','admin')) FROM workspaces ov WHERE ov.id={ws}::uuid),false))"
    )
}

/// SQL condition: `viewer` may delete or cancel an object of workspace `ws`
/// owned by `owner`, in either mode: the creating user, a service-account key
/// or a current workspace owner/admin. Personal workspaces are owner-private,
/// so whoever sees an object there may change it. Check visibility first: a
/// hidden object must stay "not found", never "forbidden".
pub(crate) fn may_modify_sql(ws: &str, viewer: &str, owner: &str) -> String {
    format!(
        "(coalesce({owner}={viewer}::uuid,false) OR coalesce((SELECT mv.kind='personal' OR {viewer}::uuid IS NULL OR EXISTS(SELECT 1 FROM effective_workspace_memberships mm WHERE mm.workspace_id=mv.id AND mm.user_id={viewer}::uuid AND mm.role IN('owner','admin')) FROM workspaces mv WHERE mv.id={ws}::uuid),false))"
    )
}

/// [`may_modify_sql`] for a known owner (a file's `created_by_user_id`).
pub(crate) async fn may_modify<'e>(
    executor: impl sqlx::PgExecutor<'e>,
    ws: Uuid,
    viewer: Viewer,
    owner: Option<Uuid>,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(&format!(
        "SELECT {}",
        may_modify_sql("$1", "$2", "$3::uuid")
    ))
    .bind(ws)
    .bind(viewer.user())
    .bind(owner)
    .fetch_one(executor)
    .await
}

/// [`may_modify_sql`] for a job of `ws` (owner: the user its key was issued to).
pub(crate) async fn may_modify_job<'e>(
    executor: impl sqlx::PgExecutor<'e>,
    ws: Uuid,
    viewer: Viewer,
    job: Uuid,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(&format!(
        "SELECT coalesce((SELECT {} FROM async_jobs j WHERE j.id=$3 AND j.workspace_id=$1),false)",
        may_modify_sql("$1", "$2", JOB_OWNER_SQL)
    ))
    .bind(ws)
    .bind(viewer.user())
    .bind(job)
    .fetch_one(executor)
    .await
}

/// The owner of `async_jobs j`: the user its key was issued to.
pub(crate) const JOB_OWNER_SQL: &str =
    "(SELECT ok.issued_to_user_id FROM api_keys ok WHERE ok.id=j.api_key_id)";

/// Whether `viewer` sees every object of workspace `ws` (the dashboard's
/// `scope` label). Same rule as [`visible_sql`].
pub(crate) async fn sees_all<'e>(
    executor: impl sqlx::PgExecutor<'e>,
    ws: Uuid,
    viewer: Viewer,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(&format!("SELECT {}", visible_sql("$1", "$2", "NULL::uuid")))
        .bind(ws)
        .bind(viewer.user())
        .fetch_one(executor)
        .await
}

/// Callers of a Team workspace for the visibility matrix tests (jobs,
/// batches, files): every caller type of the table in the module docs.
#[cfg(all(test, feature = "integration-tests"))]
pub(crate) mod test_support {
    use super::*;
    use crate::{auth::NewApiKey, governance::tests::db::Fixture};

    /// Who created an object.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(crate) enum Owner {
        /// A key issued to the regular member `member`.
        Member,
        /// A service-account key (no user owner).
        Service,
    }
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(crate) enum Who {
        Service,
        /// The workspace owner (owner role).
        Owner,
        /// A member with the admin role.
        Admin,
        /// The regular member's own key.
        Member,
        /// The regular member's next key after rotation (same lineage).
        Rotated,
        /// Another regular member.
        Other,
    }
    pub(crate) const CALLERS: [Who; 6] = [
        Who::Service,
        Who::Owner,
        Who::Admin,
        Who::Member,
        Who::Rotated,
        Who::Other,
    ];
    /// The rule under test: `workspace` shows everything; `member` shows a
    /// regular member only the objects of keys issued to them.
    pub(crate) fn expected(mode: &str, owner: Owner, who: Who) -> bool {
        mode == "workspace"
            || matches!(who, Who::Service | Who::Owner | Who::Admin)
            || (owner == Owner::Member && matches!(who, Who::Member | Who::Rotated))
    }
    /// Delete and cancel, in either mode: the creating user, a workspace
    /// owner/admin or a service-account key.
    pub(crate) fn may_modify(owner: Owner, who: Who) -> bool {
        matches!(who, Who::Service | Who::Owner | Who::Admin)
            || (owner == Owner::Member && matches!(who, Who::Member | Who::Rotated))
    }
    pub(crate) struct Caller {
        pub principal: Principal,
        pub bearer: String,
    }
    pub(crate) struct Team {
        pub ws: Uuid,
        callers: Vec<(Who, Caller)>,
    }
    impl Team {
        pub(crate) fn get(&self, who: Who) -> &Caller {
            &self.callers.iter().find(|(w, _)| *w == who).unwrap().1
        }
        pub(crate) fn owner(&self, owner: Owner) -> &Caller {
            self.get(match owner {
                Owner::Member => Who::Member,
                Owner::Service => Who::Service,
            })
        }
        pub(crate) async fn mode(&self, f: &Fixture, mode: &str) {
            sqlx::query("UPDATE workspaces SET object_visibility=$2 WHERE id=$1")
                .bind(self.ws)
                .bind(mode)
                .execute(&f.store.pool)
                .await
                .unwrap();
        }
    }
    async fn key(
        f: &Fixture,
        ws: Uuid,
        user: Option<Uuid>,
        account: Option<Uuid>,
        lineage: Option<Uuid>,
    ) -> Caller {
        let key = NewApiKey::generate().unwrap();
        sqlx::query("INSERT INTO api_keys(id,workspace_id,issued_to_user_id,service_account_id,name,secret_hash,governance_key_id) VALUES($1,$2,$3,$4,'matrix',$5,$6)")
            .bind(key.id).bind(ws).bind(user).bind(account).bind(key.digest.as_slice()).bind(lineage)
            .execute(&f.store.pool).await.unwrap();
        Caller {
            principal: Principal {
                key_id: key.id,
                workspace_id: ws,
                user_id: user,
            },
            bearer: format!("Bearer {}", key.token),
        }
    }
    async fn user(f: &Fixture, ws: Uuid, role: &str) -> Uuid {
        let id = Uuid::new_v4();
        sqlx::query("INSERT INTO users(id,email) VALUES($1,$2)")
            .bind(id)
            .bind(format!("{role}-{id}@test.invalid"))
            .execute(&f.store.pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO platform_role_grants(user_id,role,source) VALUES($1,'user','manual')",
        )
        .bind(id)
        .execute(&f.store.pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO workspace_membership_grants(workspace_id,user_id,role,source) VALUES($1,$2,$3,'manual')")
            .bind(ws).bind(id).bind(role)
            .execute(&f.store.pool).await.unwrap();
        id
    }
    /// The fixture's Team with one caller of each type and no job limit.
    pub(crate) async fn team(f: &Fixture) -> Team {
        let ws = f.team.workspace_id;
        let account = Uuid::new_v4();
        sqlx::query("INSERT INTO service_accounts(id,workspace_id,name) VALUES($1,$2,'matrix')")
            .bind(account)
            .bind(ws)
            .execute(&f.store.pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO workspace_platform_policy_overrides(workspace_id,concurrent_jobs) VALUES($1,1000)")
            .bind(ws)
            .execute(&f.store.pool)
            .await
            .unwrap();
        let admin = user(f, ws, "admin").await;
        let other = user(f, ws, "member").await;
        let member = key(f, ws, Some(f.other), None, None).await;
        let rotated = key(f, ws, Some(f.other), None, Some(member.principal.key_id)).await;
        let callers = vec![
            (Who::Service, key(f, ws, None, Some(account), None).await),
            (Who::Owner, key(f, ws, Some(f.owner), None, None).await),
            (Who::Admin, key(f, ws, Some(admin), None, None).await),
            (Who::Member, member),
            (Who::Rotated, rotated),
            (Who::Other, key(f, ws, Some(other), None, None).await),
        ];
        Team { ws, callers }
    }
}
