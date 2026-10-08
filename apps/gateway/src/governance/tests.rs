use super::*;
use crate::inference::error::LimitScope;
#[test]
fn integer_cost_rounding_overflow_and_generic_validation() {
    assert_eq!(cost(1, 1, 1, 1), Ok(2));
    assert_eq!(cost(0, 0, 1, 1), Ok(0));
    assert_eq!(cost(1_000_001, 1_000_000, 1, 1), Ok(3));
    assert_eq!(
        checked_cost(i64::MAX as u64, 1, i64::MAX, 1),
        Err(billing::BillingError::Overflow)
    );
    assert_eq!(
        cost(i64::MAX as u64, 1, i64::MAX, 1),
        Err(InferenceError::Storage)
    );
    assert_eq!(
        checked_cost(1, 1, -1, 1),
        Err(billing::BillingError::InvalidRate)
    );
    assert!(
        usage_values(Usage {
            input_tokens: Some(u64::MAX),
            ..Default::default()
        })
        .is_err()
    );
    assert!(
        usage_values(Usage {
            input_tokens: Some(11),
            billing: Some(BillingUsage {
                total_input_tokens: Some(10),
                ..Default::default()
            }),
            ..Default::default()
        })
        .is_err()
    );
}
#[test]
fn cache_bound_requires_pricing_for_every_possible_residual() {
    use CacheRate::{NotApplicable as Na, Priced, Unknown};
    let priced = Priced {
        microusd_per_million: 1_000_000,
    };
    let all_na = [Na; 4];
    let cases = [
        // D2: NA categories are impossible absent forcing evidence, so fully
        // unknown usage with a finite priced ceiling keeps a finite hold.
        ("missing metadata", None, all_na, false),
        (
            "missing metadata live GPT rates",
            None,
            [priced, priced, Na, Na],
            false,
        ),
        (
            "missing metadata unknown 5m stays unbounded",
            None,
            [priced, priced, Unknown, Na],
            true,
        ),
        ("empty billing NA rates", Some([None; 7]), all_na, false),
        ("missing metadata fully priced", None, [priced; 4], false),
        (
            "missing metadata unknown read",
            None,
            [Unknown, priced, priced, priced],
            true,
        ),
        ("complete zero", Some([Some(0); 7]), all_na, false),
        (
            "missing zero allocations",
            Some([None, Some(0), Some(0), Some(0), None, None, None]),
            all_na,
            false,
        ),
        (
            "positive aggregate all NA",
            Some([Some(10), Some(0), Some(0), Some(10), None, None, None]),
            all_na,
            true,
        ),
        (
            "positive aggregate all priced",
            Some([Some(10), Some(0), Some(0), Some(10), None, None, None]),
            [priced; 4],
            false,
        ),
        (
            "positive aggregate mixed unknown",
            Some([Some(10), Some(0), Some(0), Some(10), None, None, None]),
            [Na, priced, Unknown, Na],
            true,
        ),
        (
            "only priced part can use residual",
            Some([Some(10), Some(0), Some(0), Some(10), None, Some(0), Some(0)]),
            [Na, priced, Na, Na],
            false,
        ),
        (
            "known parts exhaust aggregate",
            Some([Some(10), Some(0), Some(0), Some(10), Some(10), None, None]),
            [Na, priced, Na, Na],
            false,
        ),
        (
            "known parts exhaust inclusive",
            Some([Some(10), Some(0), Some(0), None, Some(10), None, None]),
            [Na, priced, Na, Na],
            false,
        ),
        (
            "read residual NA",
            Some([Some(10), Some(6), None, Some(0), None, None, None]),
            all_na,
            true,
        ),
        (
            "read residual priced",
            Some([Some(10), Some(6), None, Some(0), None, None, None]),
            [priced, Na, Na, Na],
            false,
        ),
        (
            "read residual zero",
            Some([Some(10), Some(10), None, Some(0), None, None, None]),
            all_na,
            false,
        ),
        (
            "missing uncached can absorb residual",
            Some([Some(10), None, None, Some(0), None, None, None]),
            all_na,
            false,
        ),
        (
            "missing uncached only",
            Some([Some(10), None, Some(0), Some(0), None, None, None]),
            all_na,
            false,
        ),
        (
            "aggregate absent writes may use residual",
            Some([Some(10), Some(6), Some(0), None, None, None, None]),
            all_na,
            true,
        ),
        (
            "aggregate absent residual zero",
            Some([Some(10), Some(10), Some(0), None, None, None, None]),
            all_na,
            false,
        ),
        (
            "unknown inclusive NA read not forced",
            Some([None, Some(6), None, Some(0), None, None, None]),
            all_na,
            false,
        ),
        (
            "unknown inclusive unknown read may use ceiling",
            Some([None, Some(6), None, Some(0), None, None, None]),
            [Unknown, Na, Na, Na],
            true,
        ),
        (
            "known uncached exhausts ceiling",
            Some([None, Some(100), None, Some(0), None, None, None]),
            all_na,
            false,
        ),
        (
            "unknown inclusive known zero cache",
            Some([None, Some(6), Some(0), Some(0), None, None, None]),
            all_na,
            false,
        ),
        (
            "observed positive NA read",
            Some([
                Some(10),
                Some(0),
                Some(10),
                Some(0),
                Some(0),
                Some(0),
                Some(0),
            ]),
            all_na,
            true,
        ),
        (
            "observed positive NA allocation",
            Some([
                Some(10),
                Some(0),
                Some(0),
                Some(10),
                Some(10),
                Some(0),
                Some(0),
            ]),
            all_na,
            true,
        ),
        (
            "unknown inclusive NA writes not forced",
            Some([None, Some(0), Some(0), None, None, None, None]),
            all_na,
            false,
        ),
        (
            "positive aggregate priced default may absorb",
            Some([Some(10), Some(0), Some(0), Some(10), None, None, None]),
            [Na, priced, Na, Na],
            false,
        ),
        (
            "aggregate missing allocations unknown total",
            Some([None, None, None, Some(10), None, None, None]),
            all_na,
            true,
        ),
        (
            "allocations exhaust but inclusive unexplained priced",
            Some([Some(10), Some(0), Some(0), None, Some(0), Some(0), Some(0)]),
            [priced; 4],
            false,
        ),
        (
            "allocations exhaust but inclusive unexplained NA",
            Some([Some(10), Some(0), Some(0), None, Some(0), Some(0), Some(0)]),
            all_na,
            true,
        ),
    ];
    for (name, counts, r, expected) in cases {
        let b = counts.map(|n| BillingUsage {
            total_input_tokens: n[0],
            uncached_input_tokens: n[1],
            cache_read_input_tokens: n[2],
            cache_write_input_tokens: n[3],
            cache_write_default_input_tokens: n[4],
            cache_write_5m_input_tokens: n[5],
            cache_write_1h_input_tokens: n[6],
        });
        let rates = CachePricing {
            read: r[0],
            write: r[1],
            write_5m: r[2],
            write_1h: r[3],
        };
        assert_eq!(
            cache_bound_violated(b.as_ref(), &rates, 100),
            Ok(expected),
            "{name}"
        );
    }
}
#[cfg(feature = "integration-tests")]
pub(crate) mod db {
    use super::*;
    use crate::{
        auth::{NewApiKey, Principal},
        inference::repository::InferenceRepository,
    };
    use sqlx::PgPool;
    pub(crate) struct Fixture {
        pub store: Store,
        pub principal: Principal,
        pub team: Principal,
        pub deployment: Uuid,
        pub model: Uuid,
        pub owner: Uuid,
        pub other: Uuid,
    }
    impl Fixture {
        pub fn start(&self) -> ExecutionStart {
            let id = Uuid::new_v4();
            ExecutionStart {
                id,
                root_request_id: id,
                attempt_number: 1,
                principal: self.principal,
                deployment_id: self.deployment,
                provider: "openai".into(),
                model: "company/smart".into(),
                streamed: false,
                upstream_model: None,
                client: Default::default(),
            }
        }
        pub async fn price(&self, rate: i64) -> Uuid {
            let id = Uuid::new_v4();
            sqlx::query("INSERT INTO deployment_prices(id,deployment_id,input_microusd_per_million,output_microusd_per_million,input_token_limit,output_token_limit,pricing_version) VALUES($1,$2,$3,$3,100,50,1)").bind(id).bind(self.deployment).bind(rate).execute(&self.store.pool).await.unwrap();
            id
        }
        pub async fn v2(&self, rates: CachePricing) -> Uuid {
            let id = Uuid::new_v4();
            sqlx::query("INSERT INTO deployment_prices(id,deployment_id,input_microusd_per_million,output_microusd_per_million,input_token_limit,output_token_limit,pricing_version,cache_pricing) VALUES($1,$2,1000000,1000000,100,50,2,$3)").bind(id).bind(self.deployment).bind(serde_json::to_value(rates).unwrap()).execute(&self.store.pool).await.unwrap();
            id
        }
        pub(crate) async fn policy(
            &self,
            table: &str,
            requests: Option<i64>,
            tokens: Option<i64>,
            concurrency: Option<i64>,
            budget: Option<i64>,
        ) {
            let (col, id) = if table == "installation_policy" {
                ("singleton", None)
            } else {
                ("workspace_id", Some(self.principal.workspace_id))
            };
            let query = if id.is_some() {
                format!(
                    "INSERT INTO {table}({col},requests_per_minute,tokens_per_minute,concurrent_requests) VALUES($1,$2,$3,$4) ON CONFLICT({col}) DO UPDATE SET requests_per_minute=$2,tokens_per_minute=$3,concurrent_requests=$4"
                )
            } else {
                format!(
                    "INSERT INTO {table}(singleton,requests_per_minute,tokens_per_minute,concurrent_requests) VALUES(true,$1,$2,$3) ON CONFLICT(singleton) DO UPDATE SET requests_per_minute=$1,tokens_per_minute=$2,concurrent_requests=$3"
                )
            };
            if let Some(id) = id {
                sqlx::query(&query)
                    .bind(id)
                    .bind(requests)
                    .bind(tokens)
                    .bind(concurrency)
                    .execute(&self.store.pool)
                    .await
                    .unwrap();
            } else {
                sqlx::query(&query)
                    .bind(requests)
                    .bind(tokens)
                    .bind(concurrency)
                    .execute(&self.store.pool)
                    .await
                    .unwrap();
            }
            let layer = match table {
                "installation_policy" => "installation",
                "workspace_platform_policy_overrides" => "override",
                "workspace_local_policies" => "local",
                other => panic!("unsupported policy table {other}"),
            };
            crate::governance::set_test_budget(
                &self.store.pool,
                layer,
                None,
                id,
                None,
                "month",
                budget,
            )
            .await;
        }
    }
    pub(crate) async fn fixture(pool: PgPool) -> Fixture {
        let owner = Uuid::new_v4();
        let other = Uuid::new_v4();
        let ws = Uuid::new_v4();
        let team = Uuid::new_v4();
        let model = Uuid::new_v4();
        let connection = Uuid::new_v4();
        let deployment = Uuid::new_v4();
        for (id, email) in [
            (owner, "owner@test.invalid"),
            (other, "member@test.invalid"),
        ] {
            sqlx::query("INSERT INTO users(id,email) VALUES($1,$2)")
                .bind(id)
                .bind(email)
                .execute(&pool)
                .await
                .unwrap();
            sqlx::query(
                "INSERT INTO platform_role_grants(user_id,role,source) VALUES($1,$2,'manual')",
            )
            .bind(id)
            .bind(if id == owner { "admin" } else { "user" })
            .execute(&pool)
            .await
            .unwrap();
        }
        sqlx::query("INSERT INTO workspaces(id,name,kind,owner_user_id) VALUES($1,'Personal','personal',$2),($3,'Team','team',NULL)").bind(ws).bind(owner).bind(team).execute(&pool).await.unwrap();
        for (user, role) in [(owner, "owner"), (other, "member")] {
            sqlx::query("INSERT INTO workspace_membership_grants(workspace_id,user_id,role,source) VALUES($1,$2,$3,'manual')").bind(team).bind(user).bind(role).execute(&pool).await.unwrap();
        }
        sqlx::query("INSERT INTO models(id,public_name,supported_protocols) VALUES($1,'company/smart',ARRAY['chat_completions'])").bind(model).execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO provider_connections(id,name,provider,credential_ref,enabled) VALUES($1,'Cloud','openai','env:NOT_RESOLVED',true)").bind(connection).execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO deployments(id,model_id,provider_connection_id,upstream_model,enabled) VALUES($1,$2,$3,'mock-model',true)").bind(deployment).bind(model).bind(connection).execute(&pool).await.unwrap();
        for w in [ws, team] {
            sqlx::query("INSERT INTO workspace_model_grants(workspace_id,model_id,source) VALUES($1,$2,'direct')").bind(w).bind(model).execute(&pool).await.unwrap();
        }
        let mut principals = Vec::new();
        for w in [ws, team] {
            let key = NewApiKey::generate();
            sqlx::query("INSERT INTO api_keys(id,workspace_id,issued_to_user_id,name,secret_hash) VALUES($1,$2,$3,'fixture',$4)").bind(key.id).bind(w).bind(owner).bind(key.digest.as_slice()).execute(&pool).await.unwrap();
            principals.push(Principal {
                key_id: key.id,
                workspace_id: w,
                user_id: Some(owner),
            });
        }
        Fixture {
            store: Store::new(pool),
            principal: principals[0],
            team: principals[1],
            deployment,
            model,
            owner,
            other,
        }
    }
    pub fn request() -> ChatRequest {
        ChatRequest {
            model: "company/smart".into(),
            messages: vec![],
            tools: vec![],
            tool_choice: None,
            temperature: None,
            max_output_tokens: Some(10),
            stream: false,
        }
    }
    pub fn done(id: Uuid, input: Option<u64>, output: Option<u64>) -> ExecutionFinish {
        ExecutionFinish {
            id,
            outcome: Outcome::Succeeded,
            error: None,
            usage: Usage {
                input_tokens: input,
                output_tokens: output,
                billing: None,
                ..Default::default()
            },
            elapsed_ms: 10,
        }
    }
    /// Embeddings and generation are separate workloads; switch the fixture model.
    pub async fn embeddings_model(f: &Fixture) {
        sqlx::query("UPDATE models SET supported_protocols=ARRAY['embeddings'] WHERE id=$1")
            .bind(f.model)
            .execute(&f.store.pool)
            .await
            .unwrap();
    }
    pub async fn amounts(f: &Fixture, id: Uuid) -> (String, Option<i64>, Option<i64>, bool) {
        sqlx::query_as("SELECT state,held_microusd,actual_microusd,unbounded_cost FROM governance_reservations WHERE execution_id=$1").bind(id).fetch_one(&f.store.pool).await.unwrap()
    }
    fn priced(n: i64) -> CacheRate {
        CacheRate::Priced {
            microusd_per_million: n,
        }
    }
    pub fn rates() -> CachePricing {
        CachePricing {
            read: priced(100_000),
            write: priced(1_250_000),
            write_5m: priced(1_250_000),
            write_1h: priced(2_000_000),
        }
    }
    pub fn billing() -> BillingUsage {
        BillingUsage {
            total_input_tokens: Some(30),
            uncached_input_tokens: Some(10),
            cache_read_input_tokens: Some(5),
            cache_write_input_tokens: Some(15),
            cache_write_default_input_tokens: Some(3),
            cache_write_5m_input_tokens: Some(4),
            cache_write_1h_input_tokens: Some(8),
        }
    }
    #[sqlx::test(migrations = "./enterprise_migrations")]
    async fn revocation_wins_and_catalog_lock_precedes_installation(pool: PgPool) {
        let f = fixture(pool).await;
        let cached = f
            .store
            .deployments(&f.principal, "company/smart")
            .await
            .unwrap()
            .remove(0);
        let mut tx = f.store.pool.begin().await.unwrap();
        sqlx::query("SELECT id FROM installation FOR NO KEY UPDATE")
            .execute(&mut *tx)
            .await
            .unwrap();
        sqlx::query("DELETE FROM workspace_model_grants WHERE workspace_id=$1")
            .bind(f.principal.workspace_id)
            .execute(&mut *tx)
            .await
            .unwrap();
        let start = f.start();
        let req = request();
        let op = admit_for_deployment(&f.store, &start, &req, 30, &cached);
        tokio::pin!(op);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), &mut op)
                .await
                .is_err()
        );
        assert!(
            !sqlx::query_scalar::<_, bool>("SELECT pg_try_advisory_xact_lock(72419502)")
                .fetch_one(&mut *tx)
                .await
                .unwrap()
        );
        tx.commit().await.unwrap();
        assert_eq!(op.await, Err(InferenceError::ModelUnavailable));
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM inference_executions")
                .fetch_one(&f.store.pool)
                .await
                .unwrap(),
            0
        );
    }
    #[sqlx::test(migrations = "./enterprise_migrations")]
    async fn credential_shared_locks_and_all_live_authority(pool: PgPool) {
        let f = fixture(pool).await;
        let mut tx = f.store.pool.begin().await.unwrap();
        assert!(
            crate::auth::revalidate(&mut tx, &f.principal)
                .await
                .unwrap()
                .is_some()
        );
        let op = sqlx::query("UPDATE users SET disabled_at=now() WHERE id=$1")
            .bind(f.owner)
            .execute(&f.store.pool);
        tokio::pin!(op);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), &mut op)
                .await
                .is_err()
        );
        tx.commit().await.unwrap();
        op.await.unwrap();
        assert_eq!(
            admit(&f.store, &f.start(), &request(), 30).await,
            Err(InferenceError::ModelUnavailable)
        );
        sqlx::query("UPDATE users SET disabled_at=NULL WHERE id=$1")
            .bind(f.owner)
            .execute(&f.store.pool)
            .await
            .unwrap();
        for (deny, restore, id) in [
            (
                "UPDATE api_keys SET revoked_at=now() WHERE id=$1",
                "UPDATE api_keys SET revoked_at=NULL WHERE id=$1",
                f.principal.key_id,
            ),
            (
                "UPDATE workspaces SET disabled_at=now() WHERE id=$1",
                "UPDATE workspaces SET disabled_at=NULL WHERE id=$1",
                f.principal.workspace_id,
            ),
            (
                "UPDATE platform_role_grants SET revoked_at=now() WHERE user_id=$1",
                "UPDATE platform_role_grants SET revoked_at=NULL WHERE user_id=$1",
                f.owner,
            ),
        ] {
            sqlx::query(deny)
                .bind(id)
                .execute(&f.store.pool)
                .await
                .unwrap();
            assert_eq!(
                admit(&f.store, &f.start(), &request(), 30).await,
                Err(InferenceError::ModelUnavailable)
            );
            sqlx::query(restore)
                .bind(id)
                .execute(&f.store.pool)
                .await
                .unwrap();
        }
        sqlx::query("UPDATE workspace_membership_grants SET revoked_at=now() WHERE user_id=$1")
            .bind(f.owner)
            .execute(&f.store.pool)
            .await
            .unwrap();
        let mut start = f.start();
        start.principal = f.team;
        assert_eq!(
            admit(&f.store, &start, &request(), 30).await,
            Err(InferenceError::ModelUnavailable)
        );
        admit(&f.store, &f.start(), &request(), 30).await.unwrap();
    }
    #[sqlx::test(migrations = "./enterprise_migrations")]
    async fn personal_direct_grant_never_authorizes_sibling_workspace(pool: PgPool) {
        let f = fixture(pool).await;
        sqlx::query("DELETE FROM workspace_model_grants WHERE workspace_id=$1")
            .bind(f.team.workspace_id)
            .execute(&f.store.pool)
            .await
            .unwrap();
        assert_eq!(
            f.store
                .deployments(&f.principal, "company/smart")
                .await
                .unwrap()
                .len(),
            1
        );
        assert!(
            f.store
                .deployments(&f.team, "company/smart")
                .await
                .unwrap()
                .is_empty()
        );
        let mut start = f.start();
        start.principal = f.team;
        assert_eq!(
            admit(&f.store, &start, &request(), 30).await,
            Err(InferenceError::ModelUnavailable)
        );
    }
    #[sqlx::test(migrations = "./enterprise_migrations")]
    async fn service_account_requires_live_account_and_workspace_grant(pool: PgPool) {
        let f = fixture(pool).await;
        let account = Uuid::new_v4();
        let key = NewApiKey::generate();
        sqlx::query("INSERT INTO service_accounts(id,workspace_id,name) VALUES($1,$2,'service')")
            .bind(account)
            .bind(f.team.workspace_id)
            .execute(&f.store.pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO api_keys(id,workspace_id,service_account_id,name,secret_hash) VALUES($1,$2,$3,'service',$4)").bind(key.id).bind(f.team.workspace_id).bind(account).bind(key.digest.as_slice()).execute(&f.store.pool).await.unwrap();
        let mut start = f.start();
        start.principal = Principal {
            key_id: key.id,
            workspace_id: f.team.workspace_id,
            user_id: None,
        };
        sqlx::query("UPDATE service_accounts SET disabled_at=now() WHERE id=$1")
            .bind(account)
            .execute(&f.store.pool)
            .await
            .unwrap();
        assert_eq!(
            admit(&f.store, &start, &request(), 30).await,
            Err(InferenceError::ModelUnavailable)
        );
        sqlx::query("UPDATE service_accounts SET disabled_at=NULL WHERE id=$1")
            .bind(account)
            .execute(&f.store.pool)
            .await
            .unwrap();
        admit(&f.store, &start, &request(), 30).await.unwrap();
        sqlx::query("DELETE FROM workspace_model_grants WHERE workspace_id=$1")
            .bind(f.team.workspace_id)
            .execute(&f.store.pool)
            .await
            .unwrap();
        assert_eq!(admit(&f.store, &f.start(), &request(), 30).await, Ok(()));
        let mut start2 = f.start();
        start2.principal = start.principal;
        assert_eq!(
            admit(&f.store, &start2, &request(), 30).await,
            Err(InferenceError::ModelUnavailable)
        );
    }
    #[sqlx::test(migrations = "./enterprise_migrations")]
    async fn installation_caps_pool_workspaces_type_caps_do_not(pool: PgPool) {
        let f = fixture(pool).await;
        f.price(1_000_000).await;
        let mut team = f.start();
        team.principal = f.team;
        admit(&f.store, &team, &request(), 60).await.unwrap();
        for (r, t, c, b) in [
            (Some(1), None, None, None),
            (None, Some(110), None, None),
            (None, None, Some(1), None),
            (None, None, None, Some(110)),
        ] {
            f.policy("installation_policy", r, t, c, b).await;
            assert_eq!(
                admit(&f.store, &f.start(), &request(), 30).await,
                Err(if b.is_some() {
                    // Never reveals whose usage exhausted the shared ceiling.
                    InferenceError::BudgetExceeded(LimitScope::Installation)
                } else {
                    InferenceError::Busy
                })
            );
        }
        f.policy("installation_policy", None, None, None, None)
            .await;
        sqlx::query("INSERT INTO workspace_type_policies(kind,concurrent_requests) VALUES('personal',1),('team',1)").execute(&f.store.pool).await.unwrap();
        admit(&f.store, &f.start(), &request(), 30).await.unwrap();
        let mut team = f.start();
        team.principal = f.team;
        assert_eq!(
            admit(&f.store, &team, &request(), 30).await,
            Err(InferenceError::Busy)
        );
    }
    #[sqlx::test(migrations = "./enterprise_migrations")]
    async fn cached_target_and_removed_protocol_drift_reject_before_reservation(pool: PgPool) {
        let f = fixture(pool).await;
        let cached = f
            .store
            .deployments(&f.principal, "company/smart")
            .await
            .unwrap()
            .remove(0);
        sqlx::query("UPDATE deployments SET upstream_model='changed'")
            .execute(&f.store.pool)
            .await
            .unwrap();
        assert_eq!(
            admit_for_deployment(&f.store, &f.start(), &request(), 30, &cached).await,
            Err(InferenceError::Configuration)
        );
        embeddings_model(&f).await;
        let cached = f
            .store
            .deployments(&f.principal, "company/smart")
            .await
            .unwrap()
            .remove(0);
        sqlx::query("UPDATE models SET supported_protocols=ARRAY['chat_completions']")
            .execute(&f.store.pool)
            .await
            .unwrap();
        let e = EmbeddingRequest {
            model: "company/smart".into(),
            input: vec!["synthetic".into()],
            dimensions: None,
        };
        assert_eq!(
            admit_embeddings_for_deployment(&f.store, &f.start(), &e, 30, &cached).await,
            Err(InferenceError::Configuration)
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM governance_reservations")
                .fetch_one(&f.store.pool)
                .await
                .unwrap(),
            0
        );
    }
    #[sqlx::test(migrations = "./enterprise_migrations")]
    async fn each_limiter_independently_serializes_races(pool: PgPool) {
        let f = fixture(pool).await;
        f.price(1_000_000).await;
        for (r, t, c, b) in [
            (Some(1), None, None, None),
            (None, Some(110), None, None),
            (None, None, Some(1), None),
            (None, None, None, Some(110)),
        ] {
            let isolated = fixture_for_workspace(&f).await;
            isolated
                .policy("workspace_local_policies", r, t, c, b)
                .await;
            let denied = if b.is_some() {
                InferenceError::BudgetExceeded(LimitScope::Workspace)
            } else {
                InferenceError::Busy
            };
            let (a, b) = (isolated.start(), isolated.start());
            let req = request();
            let result = tokio::join!(
                admit(&isolated.store, &a, &req, 60),
                admit(&isolated.store, &b, &req, 60)
            );
            assert!(
                result == (Ok(()), Err(denied)) || result == (Err(denied), Ok(())),
                "{denied:?}"
            );
        }
    }
    async fn fixture_for_workspace(f: &Fixture) -> Fixture {
        let ws = Uuid::new_v4();
        let key = NewApiKey::generate();
        sqlx::query("INSERT INTO workspaces(id,name,kind) VALUES($1,'isolated','project')")
            .bind(ws)
            .execute(&f.store.pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO workspace_membership_grants(workspace_id,user_id,role,source) VALUES($1,$2,'owner','manual')").bind(ws).bind(f.owner).execute(&f.store.pool).await.unwrap();
        sqlx::query("INSERT INTO workspace_model_grants(workspace_id,model_id,source) VALUES($1,$2,'direct')").bind(ws).bind(f.model).execute(&f.store.pool).await.unwrap();
        sqlx::query("INSERT INTO api_keys(id,workspace_id,issued_to_user_id,name,secret_hash) VALUES($1,$2,$3,'isolated',$4)").bind(key.id).bind(ws).bind(f.owner).bind(key.digest.as_slice()).execute(&f.store.pool).await.unwrap();
        Fixture {
            store: f.store.clone(),
            principal: Principal {
                workspace_id: ws,
                key_id: key.id,
                user_id: Some(f.owner),
            },
            team: f.team,
            deployment: f.deployment,
            model: f.model,
            owner: f.owner,
            other: f.other,
        }
    }
    #[sqlx::test(migrations = "./enterprise_migrations")]
    async fn pinned_price_full_finish_idempotency_and_immutable_history(pool: PgPool) {
        let f = fixture(pool).await;
        let p = f.price(1_000_000).await;
        let start = f.start();
        admit(&f.store, &start, &request(), 30).await.unwrap();
        f.price(2_000_000).await;
        let done = done(start.id, Some(3), Some(2));
        finish(&f.store, &done).await.unwrap();
        finish(&f.store, &done).await.unwrap();
        assert_eq!(
            amounts(&f, start.id).await,
            ("settled".into(), Some(110), Some(5), false)
        );
        assert!(
            finish(&f.store, &super::db::done(start.id, Some(4), Some(2)))
                .await
                .is_err()
        );
        assert!(
            sqlx::query("UPDATE deployment_prices SET input_token_limit=200 WHERE id=$1")
                .bind(p)
                .execute(&f.store.pool)
                .await
                .is_err()
        );
        assert!(
            sqlx::query("DELETE FROM monetary_ledger WHERE execution_id=$1")
                .bind(start.id)
                .execute(&f.store.pool)
                .await
                .is_err()
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM monetary_ledger WHERE execution_id=$1"
            )
            .bind(start.id)
            .fetch_one(&f.store.pool)
            .await
            .unwrap(),
            2
        );
    }
    #[sqlx::test(migrations = "./enterprise_migrations")]
    async fn missing_usage_nonshrinking_floor_manual_resolution_and_evidence(pool: PgPool) {
        let f = fixture(pool).await;
        f.price(1_000_000).await;
        f.policy("workspace_local_policies", None, None, None, Some(110))
            .await;
        let start = f.start();
        admit(&f.store, &start, &request(), 30).await.unwrap();
        finish(&f.store, &done(start.id, Some(3), None))
            .await
            .unwrap();
        assert_eq!(
            admit(&f.store, &f.start(), &request(), 30).await,
            Err(InferenceError::BudgetExceeded(LimitScope::Workspace))
        );
        let usage = Usage {
            input_tokens: Some(3),
            output_tokens: Some(2),
            billing: None,
            ..Default::default()
        };
        assert!(
            resolve_usage(
                &f.store,
                f.principal.workspace_id,
                start.id,
                Usage {
                    input_tokens: Some(2),
                    ..usage
                },
                "receipt:1",
                f.owner
            )
            .await
            .is_err()
        );
        assert!(
            resolve_usage(
                &f.store,
                f.principal.workspace_id,
                start.id,
                usage,
                "receipt:1",
                Uuid::new_v4()
            )
            .await
            .is_err()
        );
        resolve_usage(
            &f.store,
            f.principal.workspace_id,
            start.id,
            usage,
            "receipt:1",
            f.owner,
        )
        .await
        .unwrap();
        resolve_usage(
            &f.store,
            f.principal.workspace_id,
            start.id,
            usage,
            "receipt:1",
            f.owner,
        )
        .await
        .unwrap();
        assert!(
            resolve_usage(
                &f.store,
                f.principal.workspace_id,
                start.id,
                usage,
                "changed",
                f.owner
            )
            .await
            .is_err()
        );
        assert_eq!(amounts(&f, start.id).await.2, Some(5));
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM audit_events WHERE action='usage.reconciled'"
            )
            .fetch_one(&f.store.pool)
            .await
            .unwrap(),
            1
        );
    }
    #[sqlx::test(migrations = "./enterprise_migrations")]
    async fn reconciliation_rechecks_revoked_admin_after_lock_wait(pool: PgPool) {
        let f = fixture(pool).await;
        f.price(1_000_000).await;
        let start = f.start();
        admit(&f.store, &start, &request(), 30).await.unwrap();
        finish(&f.store, &done(start.id, None, None)).await.unwrap();
        let mut tx = f.store.pool.begin().await.unwrap();
        sqlx::query("SELECT id FROM installation FOR NO KEY UPDATE")
            .execute(&mut *tx)
            .await
            .unwrap();
        sqlx::query("UPDATE platform_role_grants SET role='user' WHERE user_id=$1")
            .bind(f.owner)
            .execute(&mut *tx)
            .await
            .unwrap();
        let op = resolve_usage(
            &f.store,
            f.principal.workspace_id,
            start.id,
            Usage {
                input_tokens: Some(1),
                output_tokens: Some(1),
                billing: None,
                ..Default::default()
            },
            "receipt:1",
            f.owner,
        );
        tokio::pin!(op);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), &mut op)
                .await
                .is_err()
        );
        tx.commit().await.unwrap();
        assert_eq!(op.await, Err(InferenceError::InvalidRequest));
    }
    #[sqlx::test(migrations = "./enterprise_migrations")]
    async fn partial_usage_grows_hold_oversized_usage_rolls_back(pool: PgPool) {
        let f = fixture(pool).await;
        f.price(1_000_000).await;
        let start = f.start();
        admit(&f.store, &start, &request(), 30).await.unwrap();
        assert!(
            finish(&f.store, &done(start.id, Some(i64::MAX as u64), Some(1)))
                .await
                .is_err()
        );
        assert_eq!(amounts(&f, start.id).await.0, "pending");
        finish(&f.store, &done(start.id, Some(300), None))
            .await
            .unwrap();
        assert_eq!(
            amounts(&f, start.id).await,
            ("unknown".into(), Some(300), None, true)
        );
    }
    #[sqlx::test(migrations = "./enterprise_migrations")]
    async fn expired_leases_release_concurrency_never_money(pool: PgPool) {
        let f = fixture(pool).await;
        f.price(1_000_000).await;
        f.policy("workspace_local_policies", None, None, Some(1), None)
            .await;
        let start = f.start();
        admit(&f.store, &start, &request(), 30).await.unwrap();
        sqlx::query(
            "UPDATE governance_reservations SET lease_expires_at=now()-interval '1 second'",
        )
        .execute(&f.store.pool)
        .await
        .unwrap();
        admit(&f.store, &f.start(), &request(), 30).await.unwrap();
        assert_eq!(reconcile_expired(&f.store, 100).await.unwrap(), 1);
        assert_eq!(reconcile_expired(&f.store, 100).await.unwrap(), 0);
        assert_eq!(amounts(&f, start.id).await.1, Some(110));
        assert!(
            finish(&f.store, &done(start.id, Some(0), Some(0)))
                .await
                .is_err()
        );
    }
    #[sqlx::test(migrations = "./enterprise_migrations")]
    async fn unpriced_unknown_history_blocks_later_budget_and_token_caps(pool: PgPool) {
        let f = fixture(pool).await;
        let start = f.start();
        let mut req = request();
        req.max_output_tokens = None;
        admit(&f.store, &start, &req, 30).await.unwrap();
        finish(&f.store, &done(start.id, Some(0), Some(0)))
            .await
            .unwrap();
        assert_eq!(
            amounts(&f, start.id).await,
            ("unknown".into(), None, None, true)
        );
        f.policy(
            "workspace_local_policies",
            None,
            Some(1000),
            None,
            Some(1000),
        )
        .await;
        assert_eq!(
            admit(&f.store, &f.start(), &request(), 30).await,
            Err(InferenceError::Configuration)
        );
        f.price(1_000_000).await;
        assert_eq!(
            admit(&f.store, &f.start(), &req, 30).await,
            Err(InferenceError::Configuration)
        );
        assert_eq!(
            admit(&f.store, &f.start(), &request(), 30).await,
            Err(InferenceError::UnresolvedUsage(LimitScope::Workspace))
        );
    }
    #[sqlx::test(migrations = "./enterprise_migrations")]
    async fn failed_provider_retains_hold_actual_overspend_blocks_future(pool: PgPool) {
        let f = fixture(pool).await;
        f.price(1_000_000).await;
        f.policy("workspace_local_policies", None, None, None, Some(220))
            .await;
        let a = f.start();
        admit(&f.store, &a, &request(), 30).await.unwrap();
        let mut failed = done(a.id, Some(0), Some(0));
        failed.outcome = Outcome::Failed;
        failed.error = Some(InferenceError::UpstreamRejected);
        finish(&f.store, &failed).await.unwrap();
        assert_eq!(amounts(&f, a.id).await.1, Some(110));
        let b = f.start();
        admit(&f.store, &b, &request(), 30).await.unwrap();
        finish(&f.store, &done(b.id, Some(300), Some(10)))
            .await
            .unwrap();
        assert_eq!(amounts(&f, b.id).await.2, Some(310));
        assert_eq!(
            admit(&f.store, &f.start(), &request(), 30).await,
            Err(InferenceError::BudgetExceeded(LimitScope::Workspace))
        );
    }
    #[sqlx::test(migrations = "./enterprise_migrations")]
    async fn utc_rollover_preserves_admission_month(pool: PgPool) {
        let f = fixture(pool).await;
        f.price(1_000_000).await;
        f.policy("workspace_local_policies", Some(1), None, None, Some(110))
            .await;
        let a = f.start();
        admit(&f.store, &a, &request(), 30).await.unwrap();
        sqlx::query("UPDATE governance_reservations SET admitted_at=date_trunc('month',now(),'UTC')-interval '1 second',minute_start=date_trunc('minute',now(),'UTC')-interval '1 minute',month_start=date_trunc('month',now(),'UTC')-interval '1 month'").execute(&f.store.pool).await.unwrap();
        admit(&f.store, &f.start(), &request(), 30).await.unwrap();
        finish(&f.store, &done(a.id, Some(500), Some(1)))
            .await
            .unwrap();
        assert!(sqlx::query_scalar::<_,bool>("SELECT month_start<date_trunc('month',now(),'UTC') FROM governance_reservations WHERE execution_id=$1").bind(a.id).fetch_one(&f.store.pool).await.unwrap());
    }
    #[sqlx::test(migrations = "./enterprise_migrations")]
    async fn v2_disjoint_cache_and_embeddings_are_input_only(pool: PgPool) {
        let f = fixture(pool).await;
        f.v2(rates()).await;
        embeddings_model(&f).await;
        let deployment = f
            .store
            .deployments(&f.principal, "company/smart")
            .await
            .unwrap()
            .remove(0);
        let start = f.start();
        let req = EmbeddingRequest {
            model: "company/smart".into(),
            input: vec!["not dispatched".into()],
            dimensions: None,
        };
        admit_embeddings_for_deployment(&f.store, &start, &req, 30, &deployment)
            .await
            .unwrap();
        let reserved: i64 = sqlx::query_scalar(
            "SELECT reserved_tokens FROM governance_reservations WHERE execution_id=$1",
        )
        .bind(start.id)
        .fetch_one(&f.store.pool)
        .await
        .unwrap();
        assert_eq!(reserved, 100);
        let mut finish_record = done(start.id, Some(10), None);
        finish_record.usage.billing = Some(billing());
        finish(&f.store, &finish_record).await.unwrap();
        assert_eq!(amounts(&f, start.id).await.2, Some(36));
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT output_tokens FROM inference_executions WHERE id=$1"
            )
            .bind(start.id)
            .fetch_one(&f.store.pool)
            .await
            .unwrap(),
            0
        );
        let mut changed = finish_record;
        changed.usage.billing = Some(BillingUsage {
            cache_write_default_input_tokens: Some(4),
            cache_write_5m_input_tokens: Some(3),
            ..billing()
        });
        assert!(finish(&f.store, &changed).await.is_err());
    }
    #[sqlx::test(migrations = "./enterprise_migrations")]
    async fn unknown_cache_bounds_block_every_budget_no_budget_flag_persists(pool: PgPool) {
        let f = fixture(pool).await;
        f.v2(CachePricing {
            read: CacheRate::Unknown,
            ..rates()
        })
        .await;
        f.policy("installation_policy", None, None, None, Some(10000))
            .await;
        assert_eq!(
            admit(&f.store, &f.start(), &request(), 30).await,
            Err(InferenceError::Configuration)
        );
        f.policy("installation_policy", None, None, None, None)
            .await;
        let start = f.start();
        admit(&f.store, &start, &request(), 30).await.unwrap();
        let mut record = done(start.id, Some(10), Some(2));
        record.usage.billing = Some(billing());
        finish(&f.store, &record).await.unwrap();
        assert_eq!(
            amounts(&f, start.id).await,
            ("unknown".into(), Some(37), None, true)
        );
        f.v2(rates()).await;
        f.policy("workspace_local_policies", None, None, None, Some(10000))
            .await;
        assert_eq!(
            admit(&f.store, &f.start(), &request(), 30).await,
            Err(InferenceError::UnresolvedUsage(LimitScope::Workspace))
        );
    }
    #[sqlx::test(migrations = "./enterprise_migrations")]
    async fn positive_na_category_invalidates_bound_unknown_allocation_never_settles(pool: PgPool) {
        let f = fixture(pool).await;
        f.v2(CachePricing {
            read: CacheRate::NotApplicable,
            ..rates()
        })
        .await;
        let start = f.start();
        admit(&f.store, &start, &request(), 30).await.unwrap();
        let mut record = done(start.id, Some(10), Some(2));
        record.usage.billing = Some(billing());
        finish(&f.store, &record).await.unwrap();
        assert!(amounts(&f, start.id).await.3);
        assert!(amounts(&f, start.id).await.2.is_none());
        f.v2(rates()).await;
        let start = f.start();
        admit(&f.store, &start, &request(), 30).await.unwrap();
        let mut record = done(start.id, Some(10), Some(2));
        record.usage.billing = Some(BillingUsage {
            cache_write_1h_input_tokens: None,
            ..billing()
        });
        finish(&f.store, &record).await.unwrap();
        assert!(amounts(&f, start.id).await.2.is_none());
        let corrected = Usage {
            input_tokens: Some(10),
            output_tokens: Some(2),
            billing: Some(billing()),
            ..Default::default()
        };
        resolve_usage(
            &f.store,
            f.principal.workspace_id,
            start.id,
            corrected,
            "allocation:1",
            f.owner,
        )
        .await
        .unwrap();
        assert_eq!(amounts(&f, start.id).await.2, Some(38));
    }
    #[sqlx::test(migrations = "./enterprise_migrations")]
    async fn v1_normalized_inclusive_input_required_raw_only_compatibility_and_pin(pool: PgPool) {
        let f = fixture(pool).await;
        let price = f.price(1_000_000).await;
        let cases = [
            (crate::providers::metering::anthropic(&serde_json::json!({"input_tokens":6,"output_tokens":2})).unwrap(), "unknown", None, 110, false),
            (crate::providers::metering::anthropic(&serde_json::json!({"input_tokens":6,"output_tokens":2,"cache_read_input_tokens":4,"cache_creation_input_tokens":10})).unwrap(), "settled", Some(22), 110, false),
            (Usage { input_tokens: Some(6), output_tokens: Some(2), billing: None, ..Default::default() }, "settled", Some(8), 110, false),
            (crate::providers::metering::anthropic(&serde_json::json!({"input_tokens":6,"output_tokens":2,"cache_read_input_tokens":120})).unwrap(), "unknown", None, 128, true),
        ];
        // All attempts retain the old pin even after a more expensive price is published.
        let mut admitted = Vec::new();
        for case in cases {
            let start = f.start();
            admit(&f.store, &start, &request(), 30).await.unwrap();
            admitted.push((start.id, case));
        }
        f.price(2_000_000).await;
        for (id, (usage, state, actual, held, unbounded)) in admitted {
            let mut record = done(id, None, None);
            record.usage = usage;
            finish(&f.store, &record).await.unwrap();
            finish(&f.store, &record).await.unwrap();
            assert_eq!(
                amounts(&f, id).await,
                (state.into(), Some(held), actual, unbounded)
            );
            if actual.is_none() {
                assert_eq!(
                    resolve_usage(
                        &f.store,
                        f.principal.workspace_id,
                        id,
                        usage,
                        "incomplete-inclusive-receipt",
                        f.owner,
                    )
                    .await,
                    Err(InferenceError::Configuration)
                );
                assert_eq!(amounts(&f, id).await.0, "unknown");
            }
            let row: (Uuid, bool) = sqlx::query_as("SELECT price_id,cost_components IS NULL FROM governance_reservations WHERE execution_id=$1").bind(id).fetch_one(&f.store.pool).await.unwrap();
            assert_eq!(row, (price, true));
            assert!(sqlx::query_scalar::<_, bool>("SELECT bool_and(cost_components IS NULL) FROM monetary_ledger WHERE execution_id=$1").bind(id).fetch_one(&f.store.pool).await.unwrap());
        }
    }
    #[sqlx::test(migrations = "./enterprise_migrations")]
    async fn bedrock_missing_ttl_na_invalidates_hold_and_blocks_next_budget(pool: PgPool) {
        let f = fixture(pool).await;
        f.v2(CachePricing {
            read: CacheRate::NotApplicable,
            write: CacheRate::NotApplicable,
            write_5m: CacheRate::NotApplicable,
            write_1h: CacheRate::NotApplicable,
        })
        .await;
        f.policy("workspace_local_policies", None, None, None, Some(1000))
            .await;
        let start = f.start();
        admit(&f.store, &start, &request(), 30).await.unwrap();
        let mut record = done(start.id, None, None);
        record.usage = crate::providers::metering::bedrock(&serde_json::json!({"inputTokens":0,"outputTokens":0,"cacheReadInputTokens":0,"cacheWriteInputTokens":10})).unwrap();
        assert_eq!(record.usage.billing.unwrap().total_input_tokens, Some(10));
        assert_eq!(record.usage.billing.unwrap().write_parts(), [None; 3]);
        finish(&f.store, &record).await.unwrap();
        assert_eq!(
            amounts(&f, start.id).await,
            ("unknown".into(), Some(110), None, true)
        );
        assert_eq!(
            admit(&f.store, &f.start(), &request(), 30).await,
            Err(InferenceError::UnresolvedUsage(LimitScope::Workspace))
        );
    }
    /// F7: budget/accounting denials are distinct from rate limits, report the
    /// narrowest denying scope and take precedence over retryable rate limits.
    #[sqlx::test(migrations = "./enterprise_migrations")]
    async fn budget_denials_are_scoped_and_precede_rate_limits(pool: PgPool) {
        let f = fixture(pool).await;
        f.price(1_000_000).await;
        f.policy("workspace_local_policies", Some(1), None, None, Some(10000))
            .await;
        admit(&f.store, &f.start(), &request(), 30).await.unwrap();
        assert_eq!(
            admit(&f.store, &f.start(), &request(), 30).await,
            Err(InferenceError::Busy)
        );
        crate::governance::set_test_budget(
            &f.store.pool,
            "key",
            None,
            Some(f.principal.workspace_id),
            Some(f.principal.key_id),
            "month",
            Some(150),
        )
        .await;
        assert_eq!(
            admit(&f.store, &f.start(), &request(), 30).await,
            Err(InferenceError::BudgetExceeded(LimitScope::ApiKey))
        );
        f.policy("workspace_local_policies", None, None, None, Some(150))
            .await;
        // Key and workspace both exceed: the narrowest scope is reported.
        assert_eq!(
            admit(&f.store, &f.start(), &request(), 30).await,
            Err(InferenceError::BudgetExceeded(LimitScope::ApiKey))
        );
        for e in [
            InferenceError::BudgetExceeded(LimitScope::ApiKey),
            InferenceError::BudgetExceeded(LimitScope::Workspace),
            InferenceError::BudgetExceeded(LimitScope::Installation),
            InferenceError::UnresolvedUsage(LimitScope::Workspace),
        ] {
            assert!(e.is_budget_denial());
            assert_ne!(e.code(), InferenceError::Busy.code());
            assert!(!e.message().contains("rate limit"));
        }
    }
    /// Live acceptance D2: one failed attempt with entirely unknown usage under
    /// NA TTL rates must keep a finite hold instead of blocking the month.
    #[sqlx::test(migrations = "./enterprise_migrations")]
    async fn failed_unknown_usage_with_na_rates_keeps_finite_hold(pool: PgPool) {
        let f = fixture(pool).await;
        f.v2(CachePricing {
            write_5m: CacheRate::NotApplicable,
            write_1h: CacheRate::NotApplicable,
            ..rates()
        })
        .await;
        f.policy("workspace_local_policies", None, None, None, Some(10000))
            .await;
        let start = f.start();
        admit(&f.store, &start, &request(), 30).await.unwrap();
        let held = amounts(&f, start.id).await.1;
        assert!(held.is_some());
        let mut failed = done(start.id, None, None);
        failed.outcome = Outcome::Failed;
        failed.error = Some(InferenceError::InvalidUpstream);
        finish(&f.store, &failed).await.unwrap();
        assert_eq!(
            amounts(&f, start.id).await,
            ("unknown".into(), held, None, false)
        );
        admit(&f.store, &f.start(), &request(), 30).await.unwrap();
        // An unknown-rate category with remaining capacity is still unbounded.
        let isolated = fixture_for_workspace(&f).await;
        isolated
            .v2(CachePricing {
                write_5m: CacheRate::Unknown,
                write_1h: CacheRate::NotApplicable,
                ..rates()
            })
            .await;
        let start = isolated.start();
        admit(&isolated.store, &start, &request(), 30)
            .await
            .unwrap();
        let mut failed = done(start.id, None, None);
        failed.outcome = Outcome::Failed;
        failed.error = Some(InferenceError::InvalidUpstream);
        finish(&isolated.store, &failed).await.unwrap();
        assert!(amounts(&isolated, start.id).await.3);
    }
    #[sqlx::test(migrations = "./enterprise_migrations")]
    async fn v2_partial_residuals_preserve_only_proven_finite_holds(pool: PgPool) {
        let f = fixture(pool).await;
        let all_na = CachePricing {
            read: CacheRate::NotApplicable,
            write: CacheRate::NotApplicable,
            write_5m: CacheRate::NotApplicable,
            write_1h: CacheRate::NotApplicable,
        };
        let mixed = CachePricing {
            write: priced(1_000_000),
            ..all_na
        };
        let writes = BillingUsage {
            total_input_tokens: Some(10),
            uncached_input_tokens: Some(0),
            cache_read_input_tokens: Some(0),
            cache_write_input_tokens: Some(10),
            ..Default::default()
        };
        let cases = [
            ("all priced TTL missing", rates(), Some(writes), false),
            // Only the priced default TTL can explain the writes; NA TTLs stay impossible.
            ("mixed TTL missing", mixed, Some(writes), false),
            (
                "one priced TTL missing others zero",
                mixed,
                Some(BillingUsage {
                    cache_write_5m_input_tokens: Some(0),
                    cache_write_1h_input_tokens: Some(0),
                    ..writes
                }),
                false,
            ),
            (
                "aggregate zero missing TTL",
                all_na,
                Some(BillingUsage {
                    total_input_tokens: Some(0),
                    cache_write_input_tokens: Some(0),
                    ..writes
                }),
                false,
            ),
            (
                "NA read residual",
                all_na,
                Some(BillingUsage {
                    total_input_tokens: Some(10),
                    uncached_input_tokens: Some(6),
                    cache_read_input_tokens: None,
                    cache_write_input_tokens: Some(0),
                    ..Default::default()
                }),
                true,
            ),
            ("missing billing NA", all_na, None, false),
            ("missing billing priced", rates(), None, false),
            (
                "known zero cached categories unknown total",
                all_na,
                Some(BillingUsage {
                    cache_write_input_tokens: Some(0),
                    total_input_tokens: None,
                    ..writes
                }),
                false,
            ),
        ];
        for (name, rates, billing, unbounded) in cases {
            let isolated = fixture_for_workspace(&f).await;
            isolated.v2(rates).await;
            isolated
                .policy("workspace_local_policies", None, None, None, Some(10000))
                .await;
            let start = isolated.start();
            admit(&isolated.store, &start, &request(), 30)
                .await
                .unwrap();
            let initial = amounts(&isolated, start.id).await.1;
            let mut record = done(start.id, Some(0), Some(0));
            record.usage.billing = billing;
            finish(&isolated.store, &record).await.unwrap();
            assert_eq!(
                amounts(&isolated, start.id).await,
                ("unknown".into(), initial, None, unbounded),
                "{name}"
            );
            let next = admit(&isolated.store, &isolated.start(), &request(), 30).await;
            assert_eq!(
                next,
                if unbounded {
                    Err(InferenceError::UnresolvedUsage(LimitScope::Workspace))
                } else {
                    Ok(())
                },
                "{name}"
            );
        }
    }
    async fn overflow_price(f: &Fixture, version: i16, input_limit: i64) {
        let cache = (version == 2).then(|| CachePricing {
            read: priced(2_000_000),
            write: priced(2_000_000),
            write_5m: priced(2_000_000),
            write_1h: priced(2_000_000),
        });
        sqlx::query("INSERT INTO deployment_prices(id,deployment_id,input_microusd_per_million,output_microusd_per_million,input_token_limit,output_token_limit,pricing_version,cache_pricing) VALUES($1,$2,2000000,2000000,$3,1,$4,$5)")
            .bind(Uuid::new_v4()).bind(f.deployment).bind(input_limit).bind(version)
            .bind(cache.map(|rates| serde_json::to_value(rates).unwrap()))
            .execute(&f.store.pool).await.unwrap();
    }
    fn huge_usage() -> Usage {
        Usage {
            input_tokens: Some(i64::MAX as u64),
            output_tokens: Some(0),
            billing: Some(BillingUsage {
                total_input_tokens: Some(i64::MAX as u64),
                uncached_input_tokens: Some(i64::MAX as u64),
                cache_read_input_tokens: Some(0),
                cache_write_input_tokens: Some(0),
                cache_write_default_input_tokens: Some(0),
                cache_write_5m_input_tokens: Some(0),
                cache_write_1h_input_tokens: Some(0),
            }),
            ..Default::default()
        }
    }
    async fn assert_overflow_observation(f: &Fixture, record: &ExecutionFinish, held: i64) {
        assert_eq!(
            amounts(f, record.id).await,
            ("unknown".into(), Some(held), None, true)
        );
        let row: (String, bool, Option<i64>, Option<i64>, Option<serde_json::Value>) =
            sqlx::query_as("SELECT state,completed_at IS NOT NULL,input_tokens,output_tokens,billing_usage FROM inference_executions WHERE id=$1")
                .bind(record.id).fetch_one(&f.store.pool).await.unwrap();
        let (input, output) = usage_values(record.usage).unwrap();
        let billing = billing_json(record.usage).unwrap();
        assert_eq!(
            row,
            ("succeeded".into(), true, input, output, billing.clone())
        );
        let row: (Option<i64>, Option<i64>, Option<serde_json::Value>, Option<serde_json::Value>) =
            sqlx::query_as("SELECT input_tokens,output_tokens,billing_usage,cost_components FROM governance_reservations WHERE execution_id=$1")
                .bind(record.id).fetch_one(&f.store.pool).await.unwrap();
        assert_eq!(row, (input, output, billing.clone(), None));
        type LedgerObservation = (
            String,
            Option<i64>,
            Option<i64>,
            Option<i64>,
            Option<serde_json::Value>,
            Option<serde_json::Value>,
        );
        let rows: Vec<LedgerObservation> =
            sqlx::query_as("SELECT kind,amount_microusd,input_tokens,output_tokens,billing_usage,cost_components FROM monetary_ledger WHERE execution_id=$1 ORDER BY kind")
                .bind(record.id).fetch_all(&f.store.pool).await.unwrap();
        assert_eq!(
            rows,
            vec![
                ("hold".into(), Some(held), None, None, None, None),
                ("unknown".into(), None, input, output, billing, None),
            ]
        );
    }
    async fn overflow_finishes(f: &Fixture, cases: Vec<Usage>, held: i64) {
        let req = ChatRequest {
            max_output_tokens: Some(1),
            ..request()
        };
        f.policy(
            "workspace_local_policies",
            None,
            None,
            None,
            Some(1_000_000),
        )
        .await;
        let mut records = Vec::new();
        // Admit every case while all holds are finite, before any overflow blocks the budget.
        for usage in cases {
            let start = f.start();
            admit(&f.store, &start, &req, 30).await.unwrap();
            assert_eq!(
                amounts(f, start.id).await,
                ("pending".into(), Some(held), None, false)
            );
            records.push(ExecutionFinish {
                usage,
                ..done(start.id, None, None)
            });
        }
        for record in records {
            finish(&f.store, &record).await.unwrap();
            finish(&f.store, &record).await.unwrap();
            assert_overflow_observation(f, &record, held).await;
            assert_eq!(
                admit(&f.store, &f.start(), &req, 30).await,
                Err(InferenceError::UnresolvedUsage(LimitScope::Workspace))
            );
            let changed = ExecutionFinish {
                usage: Usage {
                    input_tokens: Some(0),
                    billing: None,
                    ..record.usage
                },
                ..record
            };
            assert_eq!(
                finish(&f.store, &changed).await,
                Err(InferenceError::Storage)
            );
        }
    }
    #[sqlx::test(migrations = "./enterprise_migrations")]
    async fn v1_monetary_overflow_retains_terminal_usage_and_blocks_budget(pool: PgPool) {
        let f = fixture(pool).await;
        overflow_price(&f, 1, 100).await;
        let usage = huge_usage();
        overflow_finishes(
            &f,
            vec![
                Usage {
                    billing: None,
                    ..usage
                },
                usage,
                Usage {
                    output_tokens: None,
                    ..usage
                },
            ],
            202,
        )
        .await;
    }
    #[sqlx::test(migrations = "./enterprise_migrations")]
    async fn v2_monetary_overflow_retains_terminal_usage_and_blocks_budget(pool: PgPool) {
        let f = fixture(pool).await;
        overflow_price(&f, 2, 100).await;
        let usage = huge_usage();
        // Each component fits, but their sum overflows. Total tokens still fit.
        let third = i64::MAX as u64 / 3;
        let split = BillingUsage {
            uncached_input_tokens: Some(third),
            cache_read_input_tokens: Some(third),
            cache_write_input_tokens: Some(third + 1),
            cache_write_default_input_tokens: Some(third + 1),
            ..usage.billing.unwrap()
        };
        overflow_finishes(
            &f,
            vec![
                usage,
                Usage {
                    billing: Some(BillingUsage {
                        total_input_tokens: None,
                        ..usage.billing.unwrap()
                    }),
                    ..usage
                },
                Usage {
                    billing: Some(split),
                    ..usage
                },
                Usage {
                    output_tokens: None,
                    billing: Some(split),
                    ..usage
                },
            ],
            1002,
        )
        .await;
    }
    #[sqlx::test(migrations = "./enterprise_migrations")]
    async fn overflow_reconciliation_is_fail_closed_and_preserves_observed_evidence(pool: PgPool) {
        let f = fixture(pool).await;
        // V1 and V2 share the reconciliation guard; exercise both pinned prices.
        let req = ChatRequest {
            max_output_tokens: Some(1),
            ..request()
        };
        let mut records = Vec::new();
        for version in [1, 2] {
            overflow_price(&f, version, 100).await;
            let start = f.start();
            admit(&f.store, &start, &req, 30).await.unwrap();
            let record = ExecutionFinish {
                usage: huge_usage(),
                ..done(start.id, None, None)
            };
            finish(&f.store, &record).await.unwrap();
            records.push((record, if version == 1 { 202 } else { 1002 }));
        }
        for (record, held) in records {
            for _ in 0..2 {
                assert_eq!(
                    resolve_usage(
                        &f.store,
                        f.principal.workspace_id,
                        record.id,
                        record.usage,
                        "receipt:overflow",
                        f.owner
                    )
                    .await,
                    Err(InferenceError::Configuration)
                );
                assert_overflow_observation(&f, &record, held).await;
            }
            // Neither raw observations nor normalized evidence may be reduced to
            // make the monetary amount fit. Rejected attempts write no ledger/audit.
            let lower_raw = Usage {
                input_tokens: Some(1),
                ..record.usage
            };
            let lower_billing = Usage {
                billing: Some(BillingUsage {
                    total_input_tokens: Some(1),
                    uncached_input_tokens: Some(1),
                    ..record.usage.billing.unwrap()
                }),
                input_tokens: Some(1),
                ..record.usage
            };
            for usage in [lower_raw, lower_billing] {
                assert_eq!(
                    resolve_usage(
                        &f.store,
                        f.principal.workspace_id,
                        record.id,
                        usage,
                        "receipt:lower",
                        f.owner
                    )
                    .await,
                    Err(InferenceError::InvalidRequest)
                );
            }
            assert_overflow_observation(&f, &record, held).await;
        }
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM audit_events WHERE action='usage.reconciled'"
            )
            .fetch_one(&f.store.pool)
            .await
            .unwrap(),
            0
        );
        f.policy("workspace_local_policies", None, None, None, Some(i64::MAX))
            .await;
        assert_eq!(
            admit(&f.store, &f.start(), &req, 30).await,
            Err(InferenceError::UnresolvedUsage(LimitScope::Workspace))
        );
    }
    #[sqlx::test(migrations = "./enterprise_migrations")]
    async fn malformed_usage_overflow_is_not_accepted_as_unknown_cost(pool: PgPool) {
        let f = fixture(pool).await;
        let req = ChatRequest {
            max_output_tokens: Some(1),
            ..request()
        };
        for version in [1, 2] {
            overflow_price(&f, version, 100).await;
            let start = f.start();
            admit(&f.store, &start, &req, 30).await.unwrap();
            for billing in [
                BillingUsage {
                    total_input_tokens: Some(i64::MAX as u64 + 1),
                    ..Default::default()
                },
                BillingUsage {
                    total_input_tokens: Some(1),
                    uncached_input_tokens: Some(2),
                    ..Default::default()
                },
            ] {
                let record = ExecutionFinish {
                    usage: Usage {
                        input_tokens: Some(0),
                        output_tokens: Some(0),
                        billing: Some(billing),
                        ..Default::default()
                    },
                    ..done(start.id, None, None)
                };
                assert_eq!(
                    finish(&f.store, &record).await,
                    Err(InferenceError::Storage)
                );
            }
            assert_eq!(
                amounts(&f, start.id).await,
                (
                    "pending".into(),
                    Some(if version == 1 { 202 } else { 1002 }),
                    None,
                    false
                )
            );
            let row: (String, Option<i64>, Option<serde_json::Value>) = sqlx::query_as(
                "SELECT state,input_tokens,billing_usage FROM inference_executions WHERE id=$1",
            )
            .bind(start.id)
            .fetch_one(&f.store.pool)
            .await
            .unwrap();
            assert_eq!(row, ("started".into(), None, None));
        }
    }
    #[sqlx::test(migrations = "./enterprise_migrations")]
    async fn nonrepresentable_reservation_is_rejected_before_dispatch(pool: PgPool) {
        let f = fixture(pool).await;
        let req = ChatRequest {
            max_output_tokens: Some(1),
            ..request()
        };
        for version in [1, 2] {
            // Token reservation itself fits; the proposed monetary hold does not.
            overflow_price(&f, version, i64::MAX / 2 + 1).await;
            assert!(admit(&f.store, &f.start(), &req, 30).await.is_err());
        }
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM inference_executions")
                .fetch_one(&f.store.pool)
                .await
                .unwrap(),
            0
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM governance_reservations")
                .fetch_one(&f.store.pool)
                .await
                .unwrap(),
            0
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM monetary_ledger")
                .fetch_one(&f.store.pool)
                .await
                .unwrap(),
            0
        );
    }
    #[sqlx::test(migrations = "./enterprise_migrations")]
    async fn raw_normalized_total_validation_applies_finish_and_reconcile(pool: PgPool) {
        let f = fixture(pool).await;
        f.v2(rates()).await;
        let start = f.start();
        admit(&f.store, &start, &request(), 30).await.unwrap();
        let mut record = done(start.id, Some(31), Some(2));
        record.usage.billing = Some(billing());
        assert!(finish(&f.store, &record).await.is_err());
        finish(&f.store, &done(start.id, Some(31), None))
            .await
            .unwrap();
        assert!(
            resolve_usage(
                &f.store,
                f.principal.workspace_id,
                start.id,
                record.usage,
                "receipt",
                f.owner
            )
            .await
            .is_err()
        );
        assert_eq!(amounts(&f, start.id).await.0, "unknown");
    }
    #[sqlx::test(migrations = "./enterprise_migrations")]
    async fn cost_center_snapshot_and_compaction_preserve_financial_dimensions(pool: PgPool) {
        let f = fixture(pool).await;
        f.price(1_000_000).await;
        let cc = Uuid::new_v4();
        sqlx::query("INSERT INTO cost_centers(id,name,code) VALUES($1,'Original','CC1')")
            .bind(cc)
            .execute(&f.store.pool)
            .await
            .unwrap();
        sqlx::query("UPDATE workspaces SET cost_center_id=$1 WHERE id=$2")
            .bind(cc)
            .bind(f.principal.workspace_id)
            .execute(&f.store.pool)
            .await
            .unwrap();
        let known = f.start();
        admit(&f.store, &known, &request(), 30).await.unwrap();
        finish(&f.store, &done(known.id, Some(2), Some(3)))
            .await
            .unwrap();
        sqlx::query("UPDATE cost_centers SET name='Renamed'")
            .execute(&f.store.pool)
            .await
            .unwrap();
        sqlx::query("UPDATE workspaces SET cost_center_id=NULL")
            .execute(&f.store.pool)
            .await
            .unwrap();
        let unknown = f.start();
        admit(&f.store, &unknown, &request(), 30).await.unwrap();
        finish(&f.store, &done(unknown.id, None, None))
            .await
            .unwrap();
        sqlx::query("UPDATE inference_executions SET completed_at=now()-interval '90 days'")
            .execute(&f.store.pool)
            .await
            .unwrap();
        assert_eq!(
            crate::maintenance::compact_history(&f.store, 30, 100)
                .await
                .unwrap(),
            1
        );
        let row:(String,String,String,bool)=sqlx::query_as("SELECT public_model,provider,cost_center_name,details_redacted_at IS NOT NULL FROM inference_executions WHERE id=$1").bind(known.id).fetch_one(&f.store.pool).await.unwrap();
        assert_eq!(
            row,
            (
                "company/smart".into(),
                "openai".into(),
                "Original".into(),
                true
            )
        );
        assert_eq!(amounts(&f, known.id).await.2, Some(5));
        assert_eq!(amounts(&f, unknown.id).await.0, "unknown");
    }
    #[sqlx::test(migrations = "./enterprise_migrations")]
    async fn key_rotation_consumption_and_restriction_lineage_do_not_reset(pool: PgPool) {
        let f = fixture(pool).await;
        sqlx::query("INSERT INTO key_policies(workspace_id,governance_key_id,requests_per_minute) VALUES($1,$2,1)").bind(f.principal.workspace_id).bind(f.principal.key_id).execute(&f.store.pool).await.unwrap();
        admit(&f.store, &f.start(), &request(), 30).await.unwrap();
        let key = NewApiKey::generate();
        sqlx::query("INSERT INTO api_keys(id,workspace_id,issued_to_user_id,name,secret_hash,governance_key_id) VALUES($1,$2,$3,'rotated',$4,$5)").bind(key.id).bind(f.principal.workspace_id).bind(f.owner).bind(key.digest.as_slice()).bind(f.principal.key_id).execute(&f.store.pool).await.unwrap();
        sqlx::query("UPDATE api_keys SET revoked_at=now() WHERE id=$1")
            .bind(f.principal.key_id)
            .execute(&f.store.pool)
            .await
            .unwrap();
        let mut start = f.start();
        start.principal.key_id = key.id;
        assert_eq!(
            admit(&f.store, &start, &request(), 30).await,
            Err(InferenceError::Busy)
        );
    }
    #[sqlx::test(migrations = "./enterprise_migrations")]
    async fn override_header_replaces_live_type_default_and_local_tightens(pool: PgPool) {
        let f = fixture(pool).await;
        sqlx::query(
            "INSERT INTO workspace_type_policies(kind,requests_per_minute) VALUES('personal',1)",
        )
        .execute(&f.store.pool)
        .await
        .unwrap();
        admit(&f.store, &f.start(), &request(), 30).await.unwrap();
        assert_eq!(
            admit(&f.store, &f.start(), &request(), 30).await,
            Err(InferenceError::Busy)
        );
        sqlx::query("INSERT INTO workspace_platform_policy_overrides(workspace_id) VALUES($1)")
            .bind(f.principal.workspace_id)
            .execute(&f.store.pool)
            .await
            .unwrap();
        admit(&f.store, &f.start(), &request(), 30).await.unwrap();
        sqlx::query("DELETE FROM workspace_platform_policy_overrides")
            .execute(&f.store.pool)
            .await
            .unwrap();
        assert_eq!(
            admit(&f.store, &f.start(), &request(), 30).await,
            Err(InferenceError::Busy)
        );
        sqlx::query("UPDATE workspace_type_policies SET requests_per_minute=100")
            .execute(&f.store.pool)
            .await
            .unwrap();
        admit(&f.store, &f.start(), &request(), 30).await.unwrap();
    }
}
