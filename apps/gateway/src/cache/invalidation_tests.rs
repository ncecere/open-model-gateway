//! Per-replica caches (scale plan P4, migration 0028) on real PostgreSQL
//! with two replicas (separate pools and caches): every domain's change
//! bumps its version once per transaction and notifies; another replica's
//! caches follow within the bound; a stale replica still denies new work
//! immediately (admission re-checks live); caches fail closed.
use super::*;
use crate::{
    auth::NewApiKey,
    governance::tests::db::{Fixture, done, fixture, request},
    inference::{
        error::{InferenceError, LimitScope},
        repository::{ExecutionStart, InferenceRepository},
    },
    notify::{self, FRESHNESS},
    store::Store,
};
use sqlx::PgPool;

const MODEL: &str = "company/smart";

pub(super) async fn replica(pool: &PgPool) -> Store {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .connect_with((*pool.connect_options()).clone())
        .await
        .unwrap();
    Store::new(pool)
}

/// A replica whose versions are confirmed now and then never learn about
/// changes (no LISTEN, no poll task): the deterministic stale case.
pub(super) async fn stale_replica(pool: &PgPool) -> Store {
    let store = replica(pool).await;
    store.caches.versions.enable();
    notify::poll_once(&store.pool, &store.caches.versions)
        .await
        .unwrap();
    store
}

pub(super) async fn issue(
    pool: &PgPool,
    workspace: Uuid,
    user: Option<Uuid>,
    account: Option<Uuid>,
) -> (String, Principal) {
    let key = NewApiKey::generate();
    sqlx::query("INSERT INTO api_keys(id,workspace_id,issued_to_user_id,service_account_id,name,secret_hash) VALUES($1,$2,$3,$4,'cache',$5)")
        .bind(key.id)
        .bind(workspace)
        .bind(user)
        .bind(account)
        .bind(key.digest.as_slice())
        .execute(pool)
        .await
        .unwrap();
    (
        key.token,
        Principal {
            key_id: key.id,
            workspace_id: workspace,
            user_id: user,
        },
    )
}

fn start(f: &Fixture, principal: Principal) -> ExecutionStart {
    ExecutionStart {
        principal,
        ..f.start()
    }
}

/// Authenticate, list candidates, plan and admit (then settle) on `store`;
/// the second pass is served from the caches.
async fn warm(f: &Fixture, store: &Store, token: &str) -> Principal {
    let mut principal = None;
    for _ in 0..2 {
        let p = store.authenticate_inference(token).await.unwrap().unwrap();
        let candidates = store.deployments(&p, MODEL).await.unwrap();
        assert_eq!(candidates.len(), 1);
        let plan = store
            .route_plan(&p, MODEL, &candidates, Uuid::new_v4())
            .await
            .unwrap();
        assert_eq!(plan.deployment_ids, [f.deployment]);
        let s = start(f, p);
        crate::governance::admit_for_deployment(store, &s, &request(), 30, &candidates[0])
            .await
            .unwrap();
        crate::governance::finish(store, &done(s.id, Some(1), Some(1)))
            .await
            .unwrap();
        principal = Some(p);
    }
    principal.unwrap()
}

async fn admit_on(f: &Fixture, store: &Store, p: Principal) -> Result<(), InferenceError> {
    let candidates = store.deployments(&p, MODEL).await?;
    let deployment = candidates.first().ok_or(InferenceError::ModelUnavailable)?;
    crate::governance::admit_for_deployment(store, &start(f, p), &request(), 30, deployment).await
}

/// Revocation and every other authorization change made on replica A blocks
/// new upstream work on replica B *immediately*, even though B's caches are
/// stale (B still authenticates the key from its cache): admission re-checks
/// the key, user, membership, workspace, entitlement, catalog and budgets
/// live under its locks.
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn changes_on_one_replica_deny_admission_immediately_on_a_stale_replica(pool: PgPool) {
    let f = fixture(pool.clone()).await;
    f.price(1_000_000).await;
    let a = replica(&pool).await.pool.clone();
    let team = f.team.workspace_id;
    let personal = f.principal.workspace_id;
    let third = Uuid::new_v4();
    sqlx::query("INSERT INTO users(id,email) VALUES($1,'third@test.invalid')")
        .bind(third)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO platform_role_grants(user_id,role,source) VALUES($1,'user','manual')")
        .bind(third)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO workspace_membership_grants(workspace_id,user_id,role,source) VALUES($1,$2,'member','manual')")
        .bind(team)
        .bind(third)
        .execute(&pool)
        .await
        .unwrap();
    let account = Uuid::new_v4();
    sqlx::query("INSERT INTO service_accounts(id,workspace_id,name) VALUES($1,$2,'robot')")
        .bind(account)
        .bind(team)
        .execute(&pool)
        .await
        .unwrap();
    type Case = (
        &'static str,
        Uuid,
        Option<Uuid>,
        Option<Uuid>,
        String,
        Option<String>,
        InferenceError,
        bool,
    );
    // The key is no longer valid: refused as authentication refuses it.
    let gone = InferenceError::Unauthenticated;
    // The key is valid but may no longer use the model.
    let deny = InferenceError::ModelUnavailable;
    let owner = Some(f.owner);
    let cases: Vec<Case> = vec![
        (
            "key revoked",
            team,
            owner,
            None,
            "UPDATE api_keys SET revoked_at=now() WHERE id=$1".into(),
            None,
            gone,
            true,
        ),
        (
            "key disabled",
            team,
            owner,
            None,
            "UPDATE api_keys SET disabled_at=now() WHERE id=$1".into(),
            None,
            gone,
            true,
        ),
        (
            "key expired",
            team,
            owner,
            None,
            "UPDATE api_keys SET expires_at=now()-interval '1 second' WHERE id=$1".into(),
            None,
            gone,
            true,
        ),
        (
            "membership revoked",
            team,
            Some(f.other),
            None,
            format!(
                "UPDATE workspace_membership_grants SET revoked_at=now() WHERE user_id='{}' AND workspace_id='{team}' AND $1 IS NOT NULL",
                f.other
            ),
            None,
            gone,
            true,
        ),
        (
            "platform role revoked",
            team,
            Some(third),
            None,
            format!(
                "UPDATE platform_role_grants SET revoked_at=now() WHERE user_id='{third}' AND $1 IS NOT NULL"
            ),
            None,
            gone,
            true,
        ),
        (
            "service account disabled",
            team,
            None,
            Some(account),
            format!(
                "UPDATE service_accounts SET disabled_at=now() WHERE id='{account}' AND $1 IS NOT NULL"
            ),
            None,
            gone,
            true,
        ),
        (
            "key restricted to no models",
            team,
            owner,
            None,
            format!(
                "INSERT INTO key_model_restrictions(workspace_id,governance_key_id) VALUES('{team}',$1)"
            ),
            None,
            deny,
            false,
        ),
        (
            "budget tightened",
            personal,
            owner,
            None,
            format!(
                "INSERT INTO policy_budgets(layer,workspace_id,period,amount_microusd) SELECT 'local','{personal}','month',0 WHERE $1 IS NOT NULL"
            ),
            Some(format!(
                "DELETE FROM policy_budgets WHERE workspace_id='{personal}'"
            )),
            InferenceError::BudgetExceeded(LimitScope::Workspace),
            false,
        ),
        (
            "model entitlement removed",
            team,
            owner,
            None,
            format!(
                "DELETE FROM workspace_model_grants WHERE workspace_id='{team}' AND $1 IS NOT NULL"
            ),
            Some(format!(
                "INSERT INTO workspace_model_grants(workspace_id,model_id,source) VALUES('{team}','{}','direct')",
                f.model
            )),
            deny,
            false,
        ),
        (
            "deployment disabled",
            team,
            owner,
            None,
            format!(
                "UPDATE deployments SET enabled=false WHERE id='{}' AND $1 IS NOT NULL",
                f.deployment
            ),
            Some(format!(
                "UPDATE deployments SET enabled=true WHERE id='{}'",
                f.deployment
            )),
            deny,
            false,
        ),
        (
            "workspace disabled",
            personal,
            owner,
            None,
            format!(
                "UPDATE workspaces SET disabled_at=now() WHERE id='{personal}' AND $1 IS NOT NULL"
            ),
            None,
            gone,
            true,
        ),
    ];
    for (name, workspace, user, service, change, cleanup, expected, auth_topic) in cases {
        let (token, _) = issue(&pool, workspace, user, service).await;
        let b = stale_replica(&pool).await;
        let p = warm(&f, &b, &token).await;
        sqlx::query(&change)
            .bind(p.key_id)
            .execute(&a)
            .await
            .unwrap();
        // B has not learned about the change: its caches still answer...
        assert_eq!(
            b.authenticate_inference(&token)
                .await
                .unwrap()
                .map(|x| x.key_id),
            Some(p.key_id),
            "{name}: stale key cache hit expected"
        );
        if !auth_topic {
            assert_eq!(
                b.deployments(&p, MODEL).await.unwrap().len(),
                1,
                "{name}: stale candidates"
            );
        }
        // ...yet the very next admission on B is refused.
        assert_eq!(admit_on(&f, &b, p).await, Err(expected), "{name}");
        if auth_topic {
            // The refusal dropped B's cached entries of the key at once,
            // before B learns about the change: live authentication now.
            assert!(
                b.authenticate_inference(&token).await.unwrap().is_none(),
                "{name}: key cache entry kept after a live refusal"
            );
            assert!(
                matches!(
                    b.deployments(&p, MODEL).await,
                    Err(InferenceError::Unauthenticated)
                ),
                "{name}"
            );
        }
        // Once B confirms the versions, authorization changes are visible
        // before admission too (live authentication refuses the key).
        notify::poll_once(&b.pool, &b.caches.versions)
            .await
            .unwrap();
        if auth_topic {
            assert!(
                b.authenticate_inference(&token).await.unwrap().is_none(),
                "{name}"
            );
        }
        if let Some(cleanup) = cleanup {
            sqlx::query(&cleanup).execute(&a).await.unwrap();
        }
    }
}

/// Every domain's change bumps its version exactly once per transaction and
/// notifies `<topic>:<version>` after commit; changes that cannot affect a
/// cached result (display names, new keys, sign-in's no-op upsert) do not.
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn every_domain_bumps_once_per_transaction_and_notifies(pool: PgPool) {
    let f = fixture(pool.clone()).await;
    let mut listener = sqlx::postgres::PgListener::connect_with(&pool)
        .await
        .unwrap();
    listener.listen(notify::CHANNEL).await.unwrap();
    let version = |topic: &'static str| {
        let pool = pool.clone();
        async move {
            sqlx::query_scalar::<_, i64>("SELECT version FROM config_versions WHERE topic=$1")
                .bind(topic)
                .fetch_one(&pool)
                .await
                .unwrap()
        }
    };
    let ws = f.principal.workspace_id;
    let key = f.principal.key_id;
    let changes: [(&str, Vec<String>); 7] = [
        ("catalog", vec![format!("UPDATE models SET display_name='Smart' WHERE id='{}'", f.model), format!("INSERT INTO deployment_routing(deployment_id,priority) VALUES('{}',1)", f.deployment)]),
        ("catalog", vec![format!("INSERT INTO deployment_prices(id,deployment_id,input_microusd_per_million,output_microusd_per_million,input_token_limit,output_token_limit,pricing_version) VALUES(gen_random_uuid(),'{}',1,1,100,50,1)", f.deployment)]),
        ("access", vec![format!("UPDATE workspace_membership_grants SET revoked_at=now() WHERE user_id='{}'", f.other), format!("DELETE FROM workspace_model_grants WHERE workspace_id='{ws}'")]),
        ("keys", vec![format!("UPDATE api_keys SET disabled_at=now() WHERE id='{key}'"), format!("UPDATE api_keys SET disabled_at=NULL WHERE id='{key}'")]),
        ("policy", vec![format!("INSERT INTO workspace_local_policies(workspace_id,requests_per_minute) VALUES('{ws}',5)"), format!("INSERT INTO policy_budgets(layer,workspace_id,period,amount_microusd) VALUES('local','{ws}','day',5)")]),
        ("settings", vec!["UPDATE installation_settings SET support_url='https://help.example.invalid' WHERE singleton".into()]),
        ("access", vec![format!("UPDATE users SET disabled_at=now() WHERE id='{}'", f.other)]),
    ];
    for (topic, statements) in changes {
        let before = version(topic).await;
        let mut tx = pool.begin().await.unwrap();
        for sql in &statements {
            sqlx::query(sql).execute(&mut *tx).await.unwrap();
        }
        // Deferred: nothing is bumped (or locked) before commit.
        assert_eq!(
            version(topic).await,
            before,
            "{topic}: bumped before commit"
        );
        tx.commit().await.unwrap();
        assert_eq!(
            version(topic).await,
            before + 1,
            "{topic}: once per transaction"
        );
        let note = tokio::time::timeout(Duration::from_secs(5), listener.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            notify::parse_payload(note.payload()),
            Some((notify::Topic::parse(topic).unwrap(), (before + 1) as u64))
        );
    }
    // A rolled-back change bumps nothing.
    let before: i64 = sqlx::query_scalar("SELECT sum(version)::bigint FROM config_versions")
        .fetch_one(&pool)
        .await
        .unwrap();
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("UPDATE api_keys SET revoked_at=now() WHERE id=$1")
        .bind(f.team.key_id)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.rollback().await.unwrap();
    // Writes that cannot change a cached result.
    for sql in [
        format!(
            "UPDATE users SET display_name='Owner' WHERE id='{}'",
            f.owner
        ),
        format!(
            "INSERT INTO workspaces(id,name,kind,owner_user_id) VALUES(gen_random_uuid(),'Personal','personal','{}') ON CONFLICT(owner_user_id) WHERE kind='personal' AND disabled_at IS NULL DO UPDATE SET owner_user_id=EXCLUDED.owner_user_id",
            f.owner
        ),
        format!(
            "INSERT INTO api_keys(id,workspace_id,issued_to_user_id,name,secret_hash) VALUES(gen_random_uuid(),'{ws}','{}','new',decode(repeat('00',32),'hex'))",
            f.owner
        ),
        "INSERT INTO users(id,email) VALUES(gen_random_uuid(),'new@test.invalid')".into(),
        format!("UPDATE workspaces SET name='Renamed' WHERE id='{ws}'"),
    ] {
        sqlx::query(&sql).execute(&pool).await.unwrap();
    }
    let after: i64 = sqlx::query_scalar("SELECT sum(version)::bigint FROM config_versions")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(after, before, "no-op writes bumped a version");
    assert!(
        tokio::time::timeout(Duration::from_millis(300), listener.recv())
            .await
            .is_err(),
        "no notification expected"
    );
    // Versions never move backwards, even for the owner.
    assert!(
        sqlx::query("UPDATE config_versions SET version=0 WHERE topic='keys'")
            .execute(&pool)
            .await
            .is_err()
    );
}

async fn until(what: &str, limit: Duration, mut check: impl AsyncFnMut() -> bool) -> Duration {
    let started = Instant::now();
    loop {
        if check().await {
            return started.elapsed();
        }
        assert!(started.elapsed() < limit, "{what}: not within {limit:?}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// With change notifications running (LISTEN plus the 1 s poll), replica
/// B's caches follow every domain's change made on replica A well within the
/// 1.5 s bound (typically milliseconds through LISTEN).
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn another_replicas_caches_follow_every_domain_within_the_bound(pool: PgPool) {
    let f = fixture(pool.clone()).await;
    f.price(1_000_000).await;
    let a = replica(&pool).await.pool.clone();
    let b = replica(&pool).await;
    let listen = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect_with((*pool.connect_options()).clone())
        .await
        .unwrap();
    let task = b.start_change_notifications(Some(listen));
    until("listener up", Duration::from_secs(5), async || {
        b.caches.versions.fresh() && b.caches.versions.listener_up()
    })
    .await;
    let bound = Duration::from_millis(1500);
    let team = f.team.workspace_id;
    // keys: a revoked key stops authenticating.
    let (token, _) = issue(&pool, team, Some(f.owner), None).await;
    let p = warm(&f, &b, &token).await;
    let hits = crate::metrics::METRICS.cache_lookups("keys", "hit");
    assert!(b.authenticate_inference(&token).await.unwrap().is_some());
    assert!(crate::metrics::METRICS.cache_lookups("keys", "hit") > hits);
    sqlx::query("UPDATE api_keys SET revoked_at=now() WHERE id=$1")
        .bind(p.key_id)
        .execute(&a)
        .await
        .unwrap();
    let keys = until("keys", bound, async || {
        b.authenticate_inference(&token).await.unwrap().is_none()
    })
    .await;
    // access: a removed member's key stops authenticating.
    let (token, _) = issue(&pool, team, Some(f.other), None).await;
    warm(&f, &b, &token).await;
    sqlx::query("UPDATE workspace_membership_grants SET revoked_at=now() WHERE user_id=$1 AND workspace_id=$2").bind(f.other).bind(team).execute(&a).await.unwrap();
    let access = until("access", bound, async || {
        b.authenticate_inference(&token).await.unwrap().is_none()
    })
    .await;
    // keys (restrictions): a deny-all restriction empties the candidates.
    let (token, _) = issue(&pool, team, Some(f.owner), None).await;
    let p = warm(&f, &b, &token).await;
    sqlx::query("INSERT INTO key_model_restrictions(workspace_id,governance_key_id) VALUES($1,$2)")
        .bind(team)
        .bind(p.key_id)
        .execute(&a)
        .await
        .unwrap();
    let restriction = until("key restriction", bound, async || {
        b.deployments(&p, MODEL).await.unwrap().is_empty()
    })
    .await;
    // access (entitlement): removing the model grant empties the candidates.
    let (token, _) = issue(&pool, f.principal.workspace_id, Some(f.owner), None).await;
    let p = warm(&f, &b, &token).await;
    sqlx::query("DELETE FROM workspace_model_grants WHERE workspace_id=$1")
        .bind(p.workspace_id)
        .execute(&a)
        .await
        .unwrap();
    let entitlement = until("entitlement", bound, async || {
        b.deployments(&p, MODEL).await.unwrap().is_empty()
    })
    .await;
    // catalog: routing configuration changes reach the planner; a disabled
    // deployment leaves the candidates.
    let (token, _) = issue(&pool, team, Some(f.owner), None).await;
    let p = warm(&f, &b, &token).await;
    let second = Uuid::new_v4();
    sqlx::query("INSERT INTO deployments(id,model_id,provider_connection_id,upstream_model,enabled) SELECT $1,model_id,provider_connection_id,'second',true FROM deployments WHERE id=$2").bind(second).bind(f.deployment).execute(&a).await.unwrap();
    sqlx::query("INSERT INTO deployment_routing(deployment_id,priority) VALUES($1,-1)")
        .bind(second)
        .execute(&a)
        .await
        .unwrap();
    let catalog = until("catalog", bound, async || {
        let candidates = b.deployments(&p, MODEL).await.unwrap();
        candidates.len() == 2
            && b.route_plan(&p, MODEL, &candidates, Uuid::new_v4())
                .await
                .unwrap()
                .deployment_ids[0]
                == second
    })
    .await;
    // policy and settings: versions only (admission reads policies live).
    for (sql, topic) in [
        (
            "UPDATE installation_settings SET support_url='https://help.example.invalid/b' WHERE singleton",
            notify::Topic::Settings,
        ),
        (
            "INSERT INTO workspace_type_policies(kind,requests_per_minute) VALUES('team',99) ON CONFLICT(kind) DO UPDATE SET requests_per_minute=99",
            notify::Topic::Policy,
        ),
    ] {
        let before = b.caches.versions.version(topic);
        sqlx::query(sql).execute(&a).await.unwrap();
        until(topic.as_str(), bound, async || {
            b.caches.versions.version(topic) > before
        })
        .await;
    }
    tracing::info!(
        ?keys,
        ?access,
        ?restriction,
        ?entitlement,
        ?catalog,
        "invalidation latency"
    );
    eprintln!(
        "invalidation latency: keys {keys:?}, access {access:?}, key restriction {restriction:?}, entitlement {entitlement:?}, catalog {catalog:?}"
    );
    task.abort();
}

/// Caches fail closed: unconfirmed versions bypass every cache (live reads,
/// so a revoked key is refused at authentication), both when the poll is
/// stale and when it fails; caching resumes once a poll succeeds. A lost
/// LISTEN connection reconnects and flushes every entry.
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn caches_fail_closed_and_flush_on_reconnect(pool: PgPool) {
    let f = fixture(pool.clone()).await;
    f.price(1_000_000).await;
    let team = f.team.workspace_id;
    // Stale versions: bypass.
    let b = stale_replica(&pool).await;
    let (token, _) = issue(&pool, team, Some(f.owner), None).await;
    let p = warm(&f, &b, &token).await;
    sqlx::query("UPDATE api_keys SET revoked_at=now() WHERE id=$1")
        .bind(p.key_id)
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        b.authenticate_inference(&token).await.unwrap().is_some(),
        "fresh: stale hit"
    );
    b.caches
        .versions
        .backdate_sync(FRESHNESS + Duration::from_millis(100));
    let bypass = crate::metrics::METRICS.cache_lookups("keys", "bypass");
    assert!(
        b.authenticate_inference(&token).await.unwrap().is_none(),
        "unconfirmed: live"
    );
    assert!(crate::metrics::METRICS.cache_lookups("keys", "bypass") > bypass);
    // A failing poll (the versions table is unreadable) also bypasses.
    let c = replica(&pool).await;
    let listen = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect_with((*pool.connect_options()).clone())
        .await
        .unwrap();
    let task = c.start_change_notifications(Some(listen));
    until("fresh", Duration::from_secs(5), async || {
        c.caches.versions.fresh() && c.caches.versions.listener_up()
    })
    .await;
    let (token, _) = issue(&pool, team, Some(f.owner), None).await;
    warm(&f, &c, &token).await;
    sqlx::query("ALTER TABLE config_versions RENAME TO config_versions_hidden")
        .execute(&pool)
        .await
        .unwrap();
    let unconfirmed = until("bypass", FRESHNESS + Duration::from_secs(3), async || {
        !c.caches.versions.fresh()
    })
    .await;
    assert!(unconfirmed >= FRESHNESS - POLL_SLACK, "{unconfirmed:?}");
    assert!(matches!(
        c.caches.keys.get(&c.caches.versions, &[0; 32]),
        Lookup::Bypass
    ));
    sqlx::query("ALTER TABLE config_versions_hidden RENAME TO config_versions")
        .execute(&pool)
        .await
        .unwrap();
    until("fresh again", Duration::from_secs(3), async || {
        c.caches.versions.fresh()
    })
    .await;
    // Reconnect: terminating the LISTEN backend flushes and reconnects.
    let warmed = issue(&pool, team, Some(f.owner), None).await.0;
    warm(&f, &c, &warmed).await;
    assert!(matches!(
        c.caches
            .keys
            .get(&c.caches.versions, &crate::auth::token_digest(&warmed)),
        Lookup::Hit(_)
    ));
    let terminated: i64 = sqlx::query_scalar("SELECT count(pg_terminate_backend(pid)) FROM pg_stat_activity WHERE datname=current_database() AND query ILIKE 'listen%' AND pid<>pg_backend_pid()")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(terminated, 1);
    until("listener down", Duration::from_secs(3), async || {
        !c.caches.versions.listener_up()
    })
    .await;
    until("listener up", Duration::from_secs(5), async || {
        c.caches.versions.listener_up()
    })
    .await;
    assert!(matches!(
        c.caches
            .keys
            .get(&c.caches.versions, &crate::auth::token_digest(&warmed)),
        Lookup::Miss(_)
    ));
    task.abort();
}

/// Poll scheduling slack for the freshness assertion.
const POLL_SLACK: Duration = Duration::from_millis(1100);
