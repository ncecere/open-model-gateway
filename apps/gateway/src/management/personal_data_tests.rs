//! #76: removing a suspended person's personal data now.
use super::*;

fn path(user: Uuid) -> String {
    format!("/api/v1/platform/users/{user}/remove-personal-data")
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn a_suspended_persons_data_is_removed_now_without_rewriting_history(pool: PgPool) {
    let f = fixture(&pool).await;
    let who = &f.outsider;
    for actor in [&f.auditor, &f.member] {
        assert_eq!(
            call(&f.s, actor, "POST", &path(who.user_id), json!({}))
                .await
                .0,
            StatusCode::FORBIDDEN
        );
    }
    assert_eq!(
        call(&f.s, &f.admin, "POST", &path(who.user_id), json!({}))
            .await
            .0,
        StatusCode::CONFLICT,
        "active: suspend first"
    );
    assert_eq!(
        call(&f.s, &f.admin, "POST", &path(Uuid::new_v4()), json!({}))
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    // Copies of the person elsewhere: an accepted invitation, an alert recipient, a filename.
    sqlx::query("INSERT INTO workspace_invitations(id,workspace_id,email,role,token_hash,expires_at,accepted_at,created_by) VALUES($1,$2,$3,'member',sha256($1::text::bytea),now()+interval '1 day',now(),$4)")
        .bind(Uuid::new_v4()).bind(f.team).bind(who.email.to_uppercase()).bind(f.owner.user_id).execute(&pool).await.unwrap();
    let rule = Uuid::new_v4();
    sqlx::query("INSERT INTO alert_rules(id,scope,kind,name,window_minutes,error_rate_percent,min_requests,notify_emails) VALUES($1,'installation','error_rate','Errors',60,50,10,$2)")
        .bind(rule).bind(vec![who.email.clone(), "ops@example.test".to_owned()]).execute(&pool).await.unwrap();
    let file = Uuid::new_v4();
    sqlx::query("INSERT INTO stored_files(id,object_key,purpose,workspace_id,created_by_user_id,backend,encryption_key_id,filename) VALUES($1,'user_file/'||$2::text||'/'||$1::text,'user_file',$2,$3,'memory','memory','pat-cv.pdf')")
        .bind(file).bind(f.team).bind(who.user_id).execute(&pool).await.unwrap();
    let ledger_before: i64 = sqlx::query_scalar("SELECT count(*) FROM monetary_ledger")
        .fetch_one(&pool)
        .await
        .unwrap();
    // Suspend, then remove now (instead of after the 30-day grace period).
    assert_eq!(
        call(
            &f.s,
            &f.admin,
            "PATCH",
            &format!("/api/v1/platform/users/{}", who.user_id),
            json!({"disabled":true})
        )
        .await
        .0,
        StatusCode::OK
    );
    // On hold: refused, nothing removed.
    let hold = Uuid::new_v4();
    sqlx::query("INSERT INTO legal_holds(id,user_id,reference) VALUES($1,$2,'CASE-3')")
        .bind(hold)
        .bind(who.user_id)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        call(&f.s, &f.admin, "POST", &path(who.user_id), json!({}))
            .await
            .0,
        StatusCode::CONFLICT,
        "on legal hold"
    );
    let email: Option<String> = sqlx::query_scalar("SELECT email FROM users WHERE id=$1")
        .bind(who.user_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(email.is_some());
    sqlx::query("UPDATE legal_holds SET released_at=now(),released_by_user_id=$2 WHERE id=$1")
        .bind(hold)
        .bind(f.admin.user_id)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        call(&f.s, &f.admin, "POST", &path(who.user_id), json!({}))
            .await
            .0,
        StatusCode::OK
    );
    let (email, name, cleaned): (Option<String>, Option<String>, bool) =
        sqlx::query_as("SELECT email,display_name,cleaned_at IS NOT NULL FROM users WHERE id=$1")
            .bind(who.user_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!((email, name, cleaned), (None, None, true));
    let invited: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM workspace_invitations WHERE lower(email)=lower($1)",
    )
    .bind(&who.email)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(invited, 0, "no invitation keeps the address");
    let recipients: Vec<String> =
        sqlx::query_scalar("SELECT notify_emails FROM alert_rules WHERE id=$1")
            .bind(rule)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        recipients,
        vec!["ops@example.test".to_owned()],
        "other recipients stay"
    );
    let filename: Option<String> =
        sqlx::query_scalar("SELECT filename FROM stored_files WHERE id=$1")
            .bind(file)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(filename, None);
    // History isn't rewritten: the ledger is as it was; audit keeps the id.
    let ledger_after: i64 = sqlx::query_scalar("SELECT count(*) FROM monetary_ledger")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(ledger_before, ledger_after);
    let actions: Vec<String> = sqlx::query_scalar("SELECT action FROM audit_events WHERE resource_id=$1 AND action IN ('user.cleaned','user.personal_data_removed') ORDER BY action").bind(who.user_id).fetch_all(&pool).await.unwrap();
    assert_eq!(
        actions,
        vec![
            "user.cleaned".to_owned(),
            "user.personal_data_removed".to_owned()
        ]
    );
    // Again: already removed.
    assert_eq!(
        call(&f.s, &f.admin, "POST", &path(who.user_id), json!({}))
            .await
            .0,
        StatusCode::CONFLICT
    );
}

/// #76 review: only this person's copies go; a held team's copies stay; keys
/// and sessions are revoked and stay revoked.
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn removal_leaves_other_people_and_held_teams_alone(pool: PgPool) {
    let f = fixture(&pool).await;
    let who = &f.outsider;
    let invite = |ws: Uuid, email: String| {
        let pool = pool.clone();
        let by = f.owner.user_id;
        async move {
            let id = Uuid::new_v4();
            sqlx::query("INSERT INTO workspace_invitations(id,workspace_id,email,role,token_hash,expires_at,accepted_at,created_by) VALUES($1,$2,$3,'member',sha256($1::text::bytea),now()+interval '1 day',now(),$4)")
                .bind(id).bind(ws).bind(email).bind(by).execute(&pool).await.unwrap();
            id
        }
    };
    let in_project = invite(f.project, who.email.clone()).await;
    let in_held_team = invite(f.team, who.email.clone()).await;
    let someone_else = invite(f.project, "someone@example.test".into()).await;
    sqlx::query("INSERT INTO legal_holds(id,workspace_id,reference) VALUES($1,$2,'TEAM-1')")
        .bind(Uuid::new_v4())
        .bind(f.team)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO browser_sessions(token_hash,user_id,csrf_hash,expires_at,verified_email) VALUES(sha256('rm'::bytea),$1,sha256('c'::bytea),now()+interval '1 hour',$2)").bind(who.user_id).bind(&who.email).execute(&pool).await.unwrap();
    assert_eq!(
        call(
            &f.s,
            &f.admin,
            "PATCH",
            &format!("/api/v1/platform/users/{}", who.user_id),
            json!({"disabled":true})
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        call(&f.s, &f.admin, "POST", &path(who.user_id), json!({}))
            .await
            .0,
        StatusCode::OK
    );
    let email = |id: Uuid| {
        let pool = pool.clone();
        async move {
            sqlx::query_scalar::<_, String>("SELECT email FROM workspace_invitations WHERE id=$1")
                .bind(id)
                .fetch_one(&pool)
                .await
                .unwrap()
        }
    };
    assert_eq!(email(in_project).await, "removed@removed.invalid");
    assert_eq!(
        email(in_held_team).await,
        who.email,
        "a held team's records stay as they are"
    );
    assert_eq!(email(someone_else).await, "someone@example.test");
    let live: i64 = sqlx::query_scalar("SELECT (SELECT count(*) FROM browser_sessions WHERE user_id=$1 AND revoked_at IS NULL)+(SELECT count(*) FROM api_keys WHERE issued_to_user_id=$1 AND revoked_at IS NULL)").bind(who.user_id).fetch_one(&pool).await.unwrap();
    assert_eq!(live, 0, "sessions and keys revoked");
    // Reactivating a cleaned person isn't possible.
    assert_ne!(
        call(
            &f.s,
            &f.admin,
            "PATCH",
            &format!("/api/v1/platform/users/{}", who.user_id),
            json!({"disabled":false})
        )
        .await
        .0,
        StatusCode::OK
    );
}
