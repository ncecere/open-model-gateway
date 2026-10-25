//! Review regressions. Real mutation paths; database-observed waits determine
//! both orderings, never sleeps or scheduling guesses.
use super::*;
use std::time::Duration;

enum Operation {
    Management {
        actor: BrowserPrincipal,
        method: &'static str,
        path: String,
        body: Value,
    },
    Scim {
        method: &'static str,
        path: String,
        body: Value,
    },
}
const ISSUER: &str = "https://request-review.example.test";
const SCIM_TOKEN: &str = "access-request-review-scim-token-0123456789";
impl Operation {
    async fn execute(self, store: Store) -> (StatusCode, Value) {
        match self {
            Self::Management {
                actor,
                method,
                path,
                body,
            } => call(&store, &actor, method, &path, body).await,
            Self::Scim { method, path, body } => {
                let config = crate::scim::ScimConfig::from_lookup(|name| match name {
                    "GATEWAY_SCIM_TOKEN_ENV" => Some("REVIEW_SCIM_TOKEN".into()),
                    "REVIEW_SCIM_TOKEN" => Some(SCIM_TOKEN.into()),
                    _ => None,
                })
                .unwrap()
                .unwrap();
                let response = crate::scim::standalone_router(
                    store,
                    config,
                    ISSUER,
                    "https://gateway.example.test",
                )
                .oneshot(
                    Request::builder()
                        .method(method)
                        .uri(path)
                        .header("authorization", format!("Bearer {SCIM_TOKEN}"))
                        .header("content-type", "application/scim+json")
                        .body(Body::from(body.to_string()))
                        .unwrap(),
                )
                .await
                .unwrap();
                let status = response.status();
                let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
                (status, serde_json::from_slice(&bytes).unwrap())
            }
        }
    }
}
fn management(
    actor: &BrowserPrincipal,
    method: &'static str,
    path: String,
    body: Value,
) -> Operation {
    Operation::Management {
        actor: actor.clone(),
        method,
        path,
        body,
    }
}
fn approval(f: &Fixture, actor: &BrowserPrincipal, id: Uuid, platform: bool) -> Operation {
    let path = if platform {
        format!("/api/v1/platform/access-requests/{id}/approve")
    } else {
        format!("/api/v1/workspaces/{}/access-requests/{id}/approve", f.team)
    };
    management(actor, "POST", path, Value::Null)
}
async fn blocked_by(pool: &PgPool, waiter: i32, blocker: i32) {
    tokio::time::timeout(Duration::from_secs(10),async {
        loop {
            let blocked:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_stat_activity a JOIN pg_locks l ON l.pid=a.pid AND NOT l.granted WHERE a.pid=$1 AND a.wait_event_type='Lock' AND $2=ANY(pg_blocking_pids(a.pid)) AND l.locktype IN ('advisory','tuple','transactionid'))")
                .bind(waiter).bind(blocker).fetch_one(pool).await.unwrap();
            if blocked {break;}
            tokio::task::yield_now().await;
        }
    }).await.expect("the specified backend never blocked on the specified predecessor");
}
type Outcome = (StatusCode, Value);
async fn ordered(pool: &PgPool, first: Operation, second: Operation) -> (Outcome, Outcome) {
    let first_pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect_with((*pool.connect_options()).clone())
        .await
        .unwrap();
    let second_pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect_with((*pool.connect_options()).clone())
        .await
        .unwrap();
    let first_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&first_pool)
        .await
        .unwrap();
    let second_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&second_pool)
        .await
        .unwrap();
    // Only the test gate takes a bare installation row lock. All operations
    // below use the real helpers' catalog → installation → authority order.
    let mut gate = crate::db::begin(pool).await.unwrap();
    sqlx::query("SELECT lock_installation()")
        .execute(&mut *gate)
        .await
        .unwrap();
    let gate_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *gate)
        .await
        .unwrap();
    let first_store = Store::new(first_pool.clone());
    let first_task = tokio::spawn(first.execute(first_store));
    blocked_by(pool, first_pid, gate_pid).await;
    let second_store = Store::new(second_pool.clone());
    let second_task = tokio::spawn(second.execute(second_store));
    blocked_by(pool, second_pid, first_pid).await;
    assert!(!first_task.is_finished() && !second_task.is_finished());
    gate.rollback().await.unwrap();
    let results = tokio::time::timeout(Duration::from_secs(10), async {
        (first_task.await.unwrap(), second_task.await.unwrap())
    })
    .await
    .unwrap();
    first_pool.close().await;
    second_pool.close().await;
    results
}
async fn state(pool: &PgPool, id: Uuid, status: &str, applications: i64) {
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT status FROM access_requests WHERE id=$1")
            .bind(id)
            .fetch_one(pool)
            .await
            .unwrap(),
        status
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM access_request_applications WHERE request_id=$1"
        )
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap(),
        applications,
        "no partial or extra application rows"
    );
    assert_eq!(sqlx::query_scalar::<_,i64>("SELECT count(*) FROM access_request_applications a JOIN access_requests r ON r.id=a.request_id AND r.requester=a.requester AND r.workspace_id=a.workspace_id JOIN workspace_membership_grants g ON g.id=a.membership_grant_id AND g.workspace_id=a.workspace_id AND g.user_id=a.requester WHERE r.id=$1 AND g.source='manual' AND g.role='member'").bind(id).fetch_one(pool).await.unwrap(),applications);
}
async fn active_grants(pool: &PgPool, ws: Uuid, user: Uuid, source: &str) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM workspace_membership_grants WHERE workspace_id=$1 AND user_id=$2 AND source=$3 AND revoked_at IS NULL").bind(ws).bind(user).bind(source).fetch_one(pool).await.unwrap()
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn access_requests_review_requester_entitlement_race_both_orderings(pool: PgPool) {
    let f = fixture(&pool).await;
    discover(&pool, f.team).await;
    for approval_first in [false, true] {
        let requester = user(&pool, &format!("entitlement-{approval_first}"), "user").await;
        let (_, r) = submit(&f, &requester, f.team, "entitlement-race").await;
        let id = request_id(&r);
        let approve = approval(&f, &f.owner, id, false);
        let revoke = management(
            &f.admin,
            "DELETE",
            format!("/api/v1/platform/users/{}/roles/user", requester.user_id),
            Value::Null,
        );
        let (a, b) = if approval_first {
            ordered(&pool, approve, revoke).await
        } else {
            let (r, a) = ordered(&pool, revoke, approve).await;
            (a, r)
        };
        assert_eq!(b.0, StatusCode::OK, "{b:?}");
        assert_eq!(
            a.0,
            if approval_first {
                StatusCode::OK
            } else {
                StatusCode::CONFLICT
            },
            "{a:?}"
        );
        state(
            &pool,
            id,
            if approval_first {
                "approved"
            } else {
                "pending"
            },
            i64::from(approval_first),
        )
        .await;
        assert_eq!(
            active_grants(&pool, f.team, requester.user_id, "manual").await,
            i64::from(approval_first)
        );
        assert_eq!(sqlx::query_scalar::<_,i64>("SELECT count(*) FROM effective_workspace_memberships WHERE workspace_id=$1 AND user_id=$2").bind(f.team).bind(requester.user_id).fetch_one(&pool).await.unwrap(),0,"entitlement loss removes effective access even after authorized application");
    }
}
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn access_requests_review_approver_demotion_race_both_orderings(pool: PgPool) {
    let f = fixture(&pool).await;
    discover(&pool, f.team).await;
    for platform in [false, true] {
        for approval_first in [false, true] {
            let tag = format!("{platform}-{approval_first}");
            let requester = user(&pool, &format!("demotion-requester-{tag}"), "user").await;
            let approver = user(
                &pool,
                &format!("demoted-approver-{tag}"),
                if platform { "admin" } else { "user" },
            )
            .await;
            if !platform {
                member(&pool, f.team, approver.user_id, "admin").await;
            }
            let (_, r) = submit(&f, &requester, f.team, "demotion-race").await;
            let id = request_id(&r);
            let approve = approval(&f, &approver, id, platform);
            let path = if platform {
                format!("/api/v1/platform/users/{}/roles/admin", approver.user_id)
            } else {
                format!(
                    "/api/v1/platform/workspaces/{}/members/{}",
                    f.team, approver.user_id
                )
            };
            let demote = management(&f.admin, "DELETE", path, Value::Null);
            let (a, d) = if approval_first {
                ordered(&pool, approve, demote).await
            } else {
                let (d, a) = ordered(&pool, demote, approve).await;
                (a, d)
            };
            assert_eq!(d.0, StatusCode::OK, "{d:?}");
            assert_eq!(
                a.0,
                if approval_first {
                    StatusCode::OK
                } else {
                    StatusCode::FORBIDDEN
                },
                "{a:?}"
            );
            state(
                &pool,
                id,
                if approval_first {
                    "approved"
                } else {
                    "pending"
                },
                i64::from(approval_first),
            )
            .await;
            assert_eq!(
                active_grants(&pool, f.team, requester.user_id, "manual").await,
                i64::from(approval_first)
            );
        }
    }
}
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn access_requests_review_manual_membership_race_and_removal_both_orderings(pool: PgPool) {
    let f = fixture(&pool).await;
    discover(&pool, f.team).await;
    for approval_first in [false, true] {
        let requester = user(&pool, &format!("manual-race-{approval_first}"), "user").await;
        let (_, r) = submit(&f, &requester, f.team, "manual-race").await;
        let id = request_id(&r);
        let approve = approval(&f, &f.owner, id, false);
        let grant = management(
            &f.admin,
            "POST",
            format!("/api/v1/platform/workspaces/{}/members", f.team),
            json!({"user_id":requester.user_id,"role":"member"}),
        );
        let (a, g) = if approval_first {
            ordered(&pool, approve, grant).await
        } else {
            let (g, a) = ordered(&pool, grant, approve).await;
            (a, g)
        };
        assert_eq!(g.0, StatusCode::OK);
        assert_eq!(
            a.0,
            if approval_first {
                StatusCode::OK
            } else {
                StatusCode::CONFLICT
            }
        );
        let remove = management(
            &f.admin,
            "DELETE",
            format!(
                "/api/v1/platform/workspaces/{}/members/{}",
                f.team, requester.user_id
            ),
            Value::Null,
        );
        assert_eq!(remove.execute(f.s.clone()).await.0, StatusCode::OK);
        // A stale pending request must not undo the later removal. An approved
        // request must keep its old linkage and never apply twice.
        assert_eq!(
            decide(&f, &f.owner, id, "approve", false).await.0,
            if approval_first {
                StatusCode::OK
            } else {
                StatusCode::CONFLICT
            }
        );
        state(
            &pool,
            id,
            if approval_first {
                "approved"
            } else {
                "pending"
            },
            i64::from(approval_first),
        )
        .await;
        assert_eq!(
            active_grants(&pool, f.team, requester.user_id, "manual").await,
            0
        );
    }
    // Specifically queue removal before/after approval after an intervening
    // grant. Neither ordering is allowed to replace that external decision.
    for removal_first in [false, true] {
        let requester = user(&pool, &format!("remove-first-{removal_first}"), "user").await;
        let (_, r) = submit(&f, &requester, f.team, "remove-race").await;
        let id = request_id(&r);
        assert_eq!(
            management(
                &f.admin,
                "POST",
                format!("/api/v1/platform/workspaces/{}/members", f.team),
                json!({"user_id":requester.user_id,"role":"member"})
            )
            .execute(f.s.clone())
            .await
            .0,
            StatusCode::OK
        );
        let approve = approval(&f, &f.owner, id, false);
        let remove = management(
            &f.admin,
            "DELETE",
            format!(
                "/api/v1/platform/workspaces/{}/members/{}",
                f.team, requester.user_id
            ),
            Value::Null,
        );
        let (a, r) = if removal_first {
            let (r, a) = ordered(&pool, remove, approve).await;
            (a, r)
        } else {
            ordered(&pool, approve, remove).await
        };
        assert_eq!(r.0, StatusCode::OK);
        assert_eq!(a.0, StatusCode::CONFLICT);
        state(&pool, id, "pending", 0).await;
        assert_eq!(
            active_grants(&pool, f.team, requester.user_id, "manual").await,
            0
        );
    }
}
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn access_requests_review_scim_membership_race_both_orderings(pool: PgPool) {
    let f = fixture(&pool).await;
    discover(&pool, f.team).await;
    for approval_first in [false, true] {
        let requester = user(&pool, &format!("scim-race-{approval_first}"), "user").await;
        let group_name = format!("access-review-{approval_first}");
        sqlx::query("INSERT INTO oidc_group_mappings(id,issuer,group_value,target_kind,workspace_id,workspace_role) VALUES($1,$2,$3,'workspace',$4,'member')").bind(Uuid::new_v4()).bind(ISSUER).bind(&group_name).bind(f.team).execute(&pool).await.unwrap();
        let (status,group)=Operation::Scim{method:"POST",path:"/scim/v2/Groups".into(),body:json!({"schemas":["urn:ietf:params:scim:schemas:core:2.0:Group"],"displayName":group_name,"members":[]})}.execute(f.s.clone()).await;
        assert_eq!(status, StatusCode::CREATED, "{group}");
        let path = format!("/scim/v2/Groups/{}", group["id"].as_str().unwrap());
        let (_, r) = submit(&f, &requester, f.team, "scim-race").await;
        let id = request_id(&r);
        let approve = approval(&f, &f.owner, id, false);
        let push = Operation::Scim {
            method: "PUT",
            path: path.clone(),
            body: json!({"schemas":["urn:ietf:params:scim:schemas:core:2.0:Group"],"displayName":group_name,"members":[{"value":requester.user_id}]}),
        };
        let (a, p) = if approval_first {
            ordered(&pool, approve, push).await
        } else {
            let (p, a) = ordered(&pool, push, approve).await;
            (a, p)
        };
        assert_eq!(p.0, StatusCode::OK, "{p:?}");
        assert_eq!(
            a.0,
            if approval_first {
                StatusCode::OK
            } else {
                StatusCode::CONFLICT
            },
            "{a:?}"
        );
        assert_eq!(
            active_grants(&pool, f.team, requester.user_id, "group").await,
            1
        );
        assert_eq!(Operation::Scim{method:"PUT",path,body:json!({"schemas":["urn:ietf:params:scim:schemas:core:2.0:Group"],"displayName":group_name,"members":[]})}.execute(f.s.clone()).await.0,StatusCode::OK);
        assert_eq!(
            decide(&f, &f.owner, id, "approve", false).await.0,
            if approval_first {
                StatusCode::OK
            } else {
                StatusCode::CONFLICT
            }
        );
        state(
            &pool,
            id,
            if approval_first {
                "approved"
            } else {
                "pending"
            },
            i64::from(approval_first),
        )
        .await;
        assert_eq!(
            active_grants(&pool, f.team, requester.user_id, "group").await,
            0
        );
        assert_eq!(
            active_grants(&pool, f.team, requester.user_id, "manual").await,
            i64::from(approval_first)
        );
    }
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn access_requests_review_scim_removal_race_both_orderings(pool: PgPool) {
    let f = fixture(&pool).await;
    discover(&pool, f.team).await;
    for removal_first in [false, true] {
        let requester = user(&pool, &format!("scim-removal-{removal_first}"), "user").await;
        let group_name = format!("access-review-removal-{removal_first}");
        sqlx::query("INSERT INTO oidc_group_mappings(id,issuer,group_value,target_kind,workspace_id,workspace_role) VALUES($1,$2,$3,'workspace',$4,'member')")
            .bind(Uuid::new_v4()).bind(ISSUER).bind(&group_name).bind(f.team).execute(&pool).await.unwrap();
        let (status,group)=Operation::Scim{method:"POST",path:"/scim/v2/Groups".into(),body:json!({"schemas":["urn:ietf:params:scim:schemas:core:2.0:Group"],"displayName":group_name,"members":[]})}.execute(f.s.clone()).await;
        assert_eq!(status, StatusCode::CREATED, "{group}");
        let path = format!("/scim/v2/Groups/{}", group["id"].as_str().unwrap());
        let (_, r) = submit(&f, &requester, f.team, "scim-removal").await;
        let id = request_id(&r);
        assert_eq!(Operation::Scim{method:"PUT",path:path.clone(),body:json!({"schemas":["urn:ietf:params:scim:schemas:core:2.0:Group"],"displayName":group_name,"members":[{"value":requester.user_id}]})}.execute(f.s.clone()).await.0,StatusCode::OK);
        let approve = approval(&f, &f.owner, id, false);
        let remove = Operation::Scim {
            method: "PUT",
            path,
            body: json!({"schemas":["urn:ietf:params:scim:schemas:core:2.0:Group"],"displayName":group_name,"members":[]}),
        };
        let (a, r) = if removal_first {
            let (r, a) = ordered(&pool, remove, approve).await;
            (a, r)
        } else {
            ordered(&pool, approve, remove).await
        };
        assert_eq!(r.0, StatusCode::OK, "{r:?}");
        assert_eq!(a.0, StatusCode::CONFLICT, "{a:?}");
        state(&pool, id, "pending", 0).await;
        assert_eq!(
            active_grants(&pool, f.team, requester.user_id, "manual").await,
            0
        );
        assert_eq!(
            active_grants(&pool, f.team, requester.user_id, "group").await,
            0
        );
    }
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn access_requests_review_old_transaction_cannot_backdate_membership_changes(pool: PgPool) {
    let f = fixture(&pool).await;
    discover(&pool, f.team).await;
    let mut old = crate::db::begin(&pool).await.unwrap();
    // Force PostgreSQL's transaction clock to precede submission. Existing
    // membership paths use now() for revoked_at, so that alone is not a fence.
    let started: chrono::DateTime<chrono::Utc> = sqlx::query_scalar("SELECT now()")
        .fetch_one(&mut *old)
        .await
        .unwrap();
    let (_, r) = submit(&f, &f.outsider, f.team, "old-transaction").await;
    let id = request_id(&r);
    let submitted: chrono::DateTime<chrono::Utc> =
        sqlx::query_scalar("SELECT created_at FROM access_requests WHERE id=$1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(started < submitted);
    sqlx::query("INSERT INTO workspace_membership_grants(workspace_id,user_id,role,source) VALUES($1,$2,'member','manual')")
        .bind(f.team).bind(f.outsider.user_id).execute(&mut *old).await.unwrap();
    sqlx::query("UPDATE workspace_membership_grants SET revoked_at=now() WHERE workspace_id=$1 AND user_id=$2")
        .bind(f.team).bind(f.outsider.user_id).execute(&mut *old).await.unwrap();
    old.commit().await.unwrap();
    assert_eq!(
        decide(&f, &f.owner, id, "approve", false).await.0,
        StatusCode::CONFLICT
    );
    state(&pool, id, "pending", 0).await;
    assert_eq!(
        active_grants(&pool, f.team, f.outsider.user_id, "manual").await,
        0
    );
    assert_eq!(
        call(
            &f.s,
            &f.outsider,
            "POST",
            &format!("/api/v1/me/access-requests/{id}/withdraw"),
            Value::Null
        )
        .await
        .0,
        StatusCode::OK
    );
    let (_, fresh) = submit(&f, &f.outsider, f.team, "fresh-after-removal").await;
    assert_eq!(
        decide(&f, &f.owner, request_id(&fresh), "approve", false)
            .await
            .0,
        StatusCode::OK
    );
    state(&pool, request_id(&fresh), "approved", 1).await;
}
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn access_requests_review_history_indexes_match_queue_order(pool: PgPool) {
    let keys:Vec<String>=sqlx::query_scalar("SELECT pg_get_indexdef(indexrelid,3,true) FROM pg_index WHERE indexrelid='access_requests_requester_clock'::regclass")
        .fetch_all(&pool).await.unwrap();
    assert_eq!(keys, vec!["id"]);
    let definition:String=sqlx::query_scalar("SELECT indexdef FROM pg_indexes WHERE schemaname='public' AND indexname='access_requests_platform_clock'")
        .fetch_one(&pool).await.unwrap();
    assert!(definition.contains("(created_at DESC, id)"), "{definition}");
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn access_requests_review_reasons_scrub_after_workspace_hold_release(pool: PgPool) {
    let f = fixture(&pool).await;
    discover(&pool, f.team).await;
    let (_, r) = submit(&f, &f.outsider, f.team, "held-cleanup").await;
    let id = request_id(&r);
    let hold = Uuid::new_v4();
    sqlx::query("INSERT INTO legal_holds(id,workspace_id,reference) VALUES($1,$2,'REVIEW-HOLD')")
        .bind(hold)
        .bind(f.team)
        .execute(&pool)
        .await
        .unwrap();
    let mut tx = crate::db::begin(&pool).await.unwrap();
    crate::lifecycle::lock(&mut tx).await.unwrap();
    crate::lifecycle::lock_users(&mut tx, &[f.outsider.user_id])
        .await
        .unwrap();
    assert!(
        crate::lifecycle::cleanup_user(&mut tx, f.outsider.user_id)
            .await
            .unwrap()
    );
    tx.commit().await.unwrap();
    crate::lifecycle::cleanup_inactive_accounts_fenced(&f.s, None)
        .await
        .unwrap();
    assert!(
        sqlx::query_scalar::<_, Option<String>>("SELECT reason FROM access_requests WHERE id=$1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(
        call(
            &f.s,
            &f.admin,
            "POST",
            &format!("/api/v1/platform/legal-holds/{hold}/release"),
            Value::Null
        )
        .await
        .0,
        StatusCode::OK
    );
    crate::lifecycle::cleanup_inactive_accounts_fenced(&f.s, None)
        .await
        .unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, Option<String>>("SELECT reason FROM access_requests WHERE id=$1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap(),
        None
    );
    assert!(
        sqlx::query_scalar::<_, bool>("SELECT cleaned_at IS NOT NULL FROM users WHERE id=$1")
            .bind(f.outsider.user_id)
            .fetch_one(&pool)
            .await
            .unwrap()
    );
}
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn access_requests_review_reason_scrub_is_bounded_and_hold_aware(pool: PgPool) {
    let f = fixture(&pool).await;
    discover(&pool, f.team).await;
    let hold = Uuid::new_v4();
    sqlx::query("INSERT INTO legal_holds(id,workspace_id,reference) VALUES($1,$2,'BOUNDED-HOLD')")
        .bind(hold)
        .bind(f.team)
        .execute(&pool)
        .await
        .unwrap();
    let users: Vec<Uuid> = sqlx::query_scalar(
        "INSERT INTO users(id) SELECT gen_random_uuid() FROM generate_series(1,101) RETURNING id",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO access_requests(requester,workspace_id,reason,idempotency_digest,payload_digest) SELECT id,$1,'Held reason',sha256('key'::bytea),sha256('payload'::bytea) FROM users WHERE id=ANY($2)").bind(f.team).bind(&users).execute(&pool).await.unwrap();
    sqlx::query("UPDATE users SET cleaned_at=clock_timestamp(),disabled_at=clock_timestamp() WHERE id=ANY($1)").bind(&users).execute(&pool).await.unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM access_requests WHERE reason IS NOT NULL"
        )
        .fetch_one(&pool)
        .await
        .unwrap(),
        101
    );
    assert_eq!(
        call(
            &f.s,
            &f.admin,
            "POST",
            &format!("/api/v1/platform/legal-holds/{hold}/release"),
            Value::Null
        )
        .await
        .0,
        StatusCode::OK
    );
    // Keep another workspace's and an active user's reason while scrubbing.
    discover(&pool, f.project).await;
    sqlx::query("INSERT INTO legal_holds(id,workspace_id,reference) VALUES($1,$2,'STILL-HELD')")
        .bind(Uuid::new_v4())
        .bind(f.project)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO access_requests(requester,workspace_id,reason,idempotency_digest,payload_digest) VALUES($1,$2,'Still held',sha256('other'::bytea),sha256('payload'::bytea))").bind(users[0]).bind(f.project).execute(&pool).await.unwrap();
    assert_eq!(
        submit(&f, &f.member, f.project, "active-reason").await.0,
        StatusCode::OK
    );
    crate::lifecycle::cleanup_inactive_accounts_fenced(&f.s, None)
        .await
        .unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM access_requests WHERE workspace_id=$1 AND reason IS NOT NULL"
        )
        .bind(f.team)
        .fetch_one(&pool)
        .await
        .unwrap(),
        1
    );
    crate::lifecycle::cleanup_inactive_accounts_fenced(&f.s, None)
        .await
        .unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM access_requests WHERE workspace_id=$1 AND reason IS NOT NULL"
        )
        .bind(f.team)
        .fetch_one(&pool)
        .await
        .unwrap(),
        0
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM access_requests WHERE workspace_id=$1 AND reason IS NOT NULL"
        )
        .bind(f.project)
        .fetch_one(&pool)
        .await
        .unwrap(),
        2
    );
}
