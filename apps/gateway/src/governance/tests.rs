use super::*;

#[test]
fn integer_cost_rounding_and_overflow() {
    let mut price = Price {
        id: Uuid::new_v4(),
        input_microusd_per_million: 1,
        output_microusd_per_million: 1,
        input_token_limit: 100,
        output_token_limit: 100,
    };
    assert_eq!(cost(1, 1, &price), Ok(2));
    assert_eq!(cost(0, 0, &price), Ok(0));
    assert_eq!(cost(1_000_001, 1_000_000, &price), Ok(3));
    price.input_microusd_per_million = i64::MAX;
    assert_eq!(cost(i64::MAX, 1, &price), Err(InferenceError::Storage));
    assert_eq!(cost(-1, 0, &price), Err(InferenceError::Storage));
    assert_eq!(
        usage_values(Usage {
            input_tokens: Some(u64::MAX),
            output_tokens: None
        }),
        Err(InferenceError::Storage)
    );
}

#[cfg(feature = "integration-tests")]
mod db {
    use super::*;
    use crate::{auth::Principal, bootstrap, config::Environment};
    use sqlx::PgPool;

    struct Fixture {
        store: Store,
        principal: Principal,
        team: Principal,
        deployment: Uuid,
    }
    impl Fixture {
        fn start(&self) -> ExecutionStart {
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
            }
        }
        async fn price(&self, rate: i64) -> Uuid {
            let id = Uuid::new_v4();
            sqlx::query("INSERT INTO deployment_prices (id,organization_id,deployment_id,input_microusd_per_million,output_microusd_per_million,input_token_limit,output_token_limit) VALUES ($1,$2,$3,$4,$4,100,50)")
                .bind(id).bind(self.principal.organization_id).bind(self.deployment).bind(rate).execute(&self.store.pool).await.unwrap();
            id
        }
        async fn policy(
            &self,
            scope: &str,
            requests: Option<i64>,
            tokens: Option<i64>,
            concurrency: Option<i64>,
            budget: Option<i64>,
        ) {
            sqlx::query("INSERT INTO governance_policies (id,organization_id,scope,workspace_id,api_key_id,requests_per_minute,tokens_per_minute,concurrent_requests,monthly_budget_microusd) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9)")
                .bind(Uuid::new_v4()).bind(self.principal.organization_id).bind(scope)
                .bind(if scope == "organization" { None } else { Some(self.principal.workspace_id) })
                .bind(if scope == "key" { Some(self.principal.key_id) } else { None })
                .bind(requests).bind(tokens).bind(concurrency).bind(budget).execute(&self.store.pool).await.unwrap();
        }
    }
    async fn fixture(pool: PgPool) -> Fixture {
        let store = Store::new(pool);
        let keys = bootstrap::seed(&store, Environment::Development)
            .await
            .unwrap()
            .unwrap();
        let principal = store
            .authenticate(&keys.personal_key.token)
            .await
            .unwrap()
            .unwrap();
        let team = store
            .authenticate(&keys.team_key.token)
            .await
            .unwrap()
            .unwrap();
        sqlx::query("UPDATE users SET platform_admin=true WHERE id=$1")
            .bind(principal.user_id)
            .execute(&store.pool)
            .await
            .unwrap();
        sqlx::query("UPDATE deployments SET enabled=true")
            .execute(&store.pool)
            .await
            .unwrap();
        sqlx::query("UPDATE provider_connections SET enabled=true")
            .execute(&store.pool)
            .await
            .unwrap();
        let deployment = sqlx::query_scalar("SELECT id FROM deployments LIMIT 1")
            .fetch_one(&store.pool)
            .await
            .unwrap();
        Fixture {
            store,
            principal,
            team,
            deployment,
        }
    }
    fn request() -> ChatRequest {
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
    fn done(id: Uuid, input: Option<u64>, output: Option<u64>) -> ExecutionFinish {
        ExecutionFinish {
            id,
            outcome: Outcome::Succeeded,
            error: None,
            usage: Usage {
                input_tokens: input,
                output_tokens: output,
            },
            elapsed_ms: 10,
        }
    }
    async fn amounts(f: &Fixture, id: Uuid) -> (String, Option<i64>, Option<i64>) {
        sqlx::query_as("SELECT state,held_microusd,actual_microusd FROM governance_reservations WHERE execution_id=$1")
            .bind(id).fetch_one(&f.store.pool).await.unwrap()
    }

    #[sqlx::test]
    async fn revocation_wins_while_admission_waits_for_organization_lock(pool: PgPool) {
        use crate::inference::repository::InferenceRepository;
        let f = fixture(pool).await;
        let cached = f
            .store
            .deployments(&f.principal, "company/smart")
            .await
            .unwrap()
            .remove(0);
        let mut tx = f.store.pool.begin().await.unwrap();
        sqlx::query("SELECT pg_advisory_xact_lock_shared(72419502)")
            .execute(&mut *tx)
            .await
            .unwrap();
        sqlx::query("SELECT id FROM organizations WHERE id=$1 FOR NO KEY UPDATE")
            .bind(f.principal.organization_id)
            .execute(&mut *tx)
            .await
            .unwrap();
        sqlx::query("DELETE FROM workspace_model_grants WHERE organization_id=$1")
            .bind(f.principal.organization_id)
            .execute(&mut *tx)
            .await
            .unwrap();
        sqlx::query("DELETE FROM organization_model_grants WHERE organization_id=$1")
            .bind(f.principal.organization_id)
            .execute(&mut *tx)
            .await
            .unwrap();
        let start = f.start();
        let request = request();
        let op = admit_for_deployment(&f.store, &start, &request, 30, &cached);
        tokio::pin!(op);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), &mut op)
                .await
                .is_err()
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

    #[sqlx::test]
    async fn admission_takes_shared_catalog_lock_before_waiting_on_org(pool: PgPool) {
        let f = fixture(pool).await;
        let mut tx = f.store.pool.begin().await.unwrap();
        sqlx::query("SELECT id FROM organizations WHERE id=$1 FOR NO KEY UPDATE")
            .bind(f.principal.organization_id)
            .execute(&mut *tx)
            .await
            .unwrap();
        let start = f.start();
        let request = request();
        let admission = admit(&f.store, &start, &request, 30);
        tokio::pin!(admission);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), &mut admission)
                .await
                .is_err()
        );
        let exclusive: bool = sqlx::query_scalar("SELECT pg_try_advisory_xact_lock(72419502)")
            .fetch_one(&mut *tx)
            .await
            .unwrap();
        assert!(
            !exclusive,
            "waiting admission must already hold the shared catalog lock"
        );
        tx.commit().await.unwrap();
        admission.await.unwrap();
    }

    #[sqlx::test]
    async fn credential_shared_locks_serialize_non_key_revocation(pool: PgPool) {
        let f = fixture(pool).await;
        let mut tx = f.store.pool.begin().await.unwrap();
        assert!(
            crate::auth::revalidate(&mut tx, &f.principal)
                .await
                .unwrap()
                .is_some()
        );
        let revoke = sqlx::query("UPDATE users SET disabled_at=now() WHERE id=$1")
            .bind(f.principal.user_id)
            .execute(&f.store.pool);
        tokio::pin!(revoke);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), &mut revoke)
                .await
                .is_err()
        );
        tx.commit().await.unwrap();
        revoke.await.unwrap();
        assert_eq!(
            admit(&f.store, &f.start(), &request(), 30).await,
            Err(InferenceError::ModelUnavailable)
        );
    }

    #[sqlx::test]
    async fn admission_revalidates_all_current_credential_authority(pool: PgPool) {
        let f = fixture(pool).await;
        let org = f.principal.organization_id;
        let user = f.principal.user_id.unwrap();
        for (deny, restore, id) in [
            (
                "UPDATE api_keys SET revoked_at=now() WHERE id=$1",
                "UPDATE api_keys SET revoked_at=NULL WHERE id=$1",
                f.principal.key_id,
            ),
            (
                "UPDATE users SET disabled_at=now() WHERE id=$1",
                "UPDATE users SET disabled_at=NULL WHERE id=$1",
                user,
            ),
            (
                "UPDATE workspaces SET disabled_at=now() WHERE id=$1",
                "UPDATE workspaces SET disabled_at=NULL WHERE id=$1",
                f.principal.workspace_id,
            ),
            (
                "UPDATE organizations SET disabled_at=now() WHERE id=$1",
                "UPDATE organizations SET disabled_at=NULL WHERE id=$1",
                org,
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
        sqlx::query("UPDATE organization_memberships SET disabled_at=now() WHERE organization_id=$1 AND user_id=$2")
            .bind(org).bind(user).execute(&f.store.pool).await.unwrap();
        assert_eq!(
            admit(&f.store, &f.start(), &request(), 30).await,
            Err(InferenceError::ModelUnavailable)
        );
        sqlx::query("UPDATE organization_memberships SET disabled_at=NULL WHERE organization_id=$1 AND user_id=$2")
            .bind(org).bind(user).execute(&f.store.pool).await.unwrap();
        sqlx::query("UPDATE workspace_memberships SET disabled_at=now() WHERE organization_id=$1 AND workspace_id=$2")
            .bind(org).bind(f.team.workspace_id).execute(&f.store.pool).await.unwrap();
        let mut team = f.start();
        team.principal = f.team;
        assert_eq!(
            admit(&f.store, &team, &request(), 30).await,
            Err(InferenceError::ModelUnavailable)
        );
        admit(&f.store, &f.start(), &request(), 30).await.unwrap();
    }

    #[sqlx::test]
    async fn individual_grants_only_authorize_own_personal_workspace(pool: PgPool) {
        use crate::inference::repository::InferenceRepository;
        let f = fixture(pool).await;
        let model: Uuid = sqlx::query_scalar("SELECT model_id FROM deployments WHERE id=$1")
            .bind(f.deployment)
            .fetch_one(&f.store.pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM workspace_model_grants WHERE organization_id=$1")
            .bind(f.principal.organization_id)
            .execute(&f.store.pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO user_model_grants (organization_id,user_id,model_id) VALUES ($1,$2,$3)",
        )
        .bind(f.principal.organization_id)
        .bind(f.principal.user_id)
        .bind(model)
        .execute(&f.store.pool)
        .await
        .unwrap();
        assert_eq!(f.store.visible_models(&f.principal).await.unwrap().len(), 1);
        assert_eq!(
            f.store
                .deployments(&f.principal, "company/smart")
                .await
                .unwrap()
                .len(),
            1
        );
        assert!(f.store.visible_models(&f.team).await.unwrap().is_empty());
        assert!(
            f.store
                .deployments(&f.team, "company/smart")
                .await
                .unwrap()
                .is_empty()
        );
        admit(&f.store, &f.start(), &request(), 30).await.unwrap();
        let mut team = f.start();
        team.principal = f.team;
        assert_eq!(
            admit(&f.store, &team, &request(), 30).await,
            Err(InferenceError::ModelUnavailable)
        );
        sqlx::query("DELETE FROM user_model_grants WHERE organization_id=$1")
            .bind(f.principal.organization_id)
            .execute(&f.store.pool)
            .await
            .unwrap();
        assert_eq!(
            admit(&f.store, &f.start(), &request(), 30).await,
            Err(InferenceError::ModelUnavailable)
        );
    }

    #[sqlx::test]
    async fn service_keys_need_workspace_grants_and_live_service_accounts(pool: PgPool) {
        let f = fixture(pool).await;
        let account = Uuid::new_v4();
        let key = crate::auth::NewApiKey::generate();
        sqlx::query("INSERT INTO service_accounts (id,organization_id,workspace_id,name) VALUES ($1,$2,$3,'service')")
            .bind(account).bind(f.team.organization_id).bind(f.team.workspace_id).execute(&f.store.pool).await.unwrap();
        sqlx::query("INSERT INTO api_keys (id,organization_id,workspace_id,service_account_id,name,secret_hash) VALUES ($1,$2,$3,$4,'service',$5)")
            .bind(key.id).bind(f.team.organization_id).bind(f.team.workspace_id).bind(account).bind(key.digest.as_slice()).execute(&f.store.pool).await.unwrap();
        let principal = f.store.authenticate(&key.token).await.unwrap().unwrap();
        let mut start = f.start();
        start.principal = principal;
        sqlx::query(
            "DELETE FROM workspace_model_grants WHERE organization_id=$1 AND workspace_id=$2",
        )
        .bind(principal.organization_id)
        .bind(principal.workspace_id)
        .execute(&f.store.pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO user_model_grants (organization_id,user_id,model_id) SELECT $1,$2,model_id FROM deployments WHERE id=$3")
            .bind(principal.organization_id).bind(f.principal.user_id).bind(f.deployment).execute(&f.store.pool).await.unwrap();
        assert_eq!(
            admit(&f.store, &start, &request(), 30).await,
            Err(InferenceError::ModelUnavailable)
        );
        sqlx::query("INSERT INTO workspace_model_grants (organization_id,workspace_id,model_id) SELECT $1,$2,model_id FROM deployments WHERE id=$3")
            .bind(principal.organization_id).bind(principal.workspace_id).bind(f.deployment).execute(&f.store.pool).await.unwrap();
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
    }

    #[sqlx::test]
    async fn platform_parent_cap_aggregates_workspaces_and_cannot_be_bypassed(pool: PgPool) {
        let f = fixture(pool).await;
        f.policy("organization", Some(100), None, Some(100), None)
            .await;
        f.policy("workspace", Some(100), None, Some(100), None)
            .await;
        f.policy("key", Some(100), None, Some(100), None).await;
        sqlx::query("INSERT INTO platform_organization_policies (organization_id,concurrent_requests) VALUES ($1,1)")
            .bind(f.principal.organization_id).execute(&f.store.pool).await.unwrap();
        let mut team = f.start();
        team.principal = f.team;
        admit(&f.store, &team, &request(), 30).await.unwrap();
        assert_eq!(
            admit(&f.store, &f.start(), &request(), 30).await,
            Err(InferenceError::Busy)
        );
        finish(&f.store, &done(team.id, None, None)).await.unwrap();
        admit(&f.store, &f.start(), &request(), 30).await.unwrap();
    }

    #[sqlx::test]
    async fn every_platform_cap_counts_other_workspaces_without_child_configuration(pool: PgPool) {
        let f = fixture(pool).await;
        f.price(1_000_000).await;
        let mut team = f.start();
        team.principal = f.team;
        admit(&f.store, &team, &request(), 60).await.unwrap();
        for (requests, tokens, concurrency, budget) in [
            (Some(1_i64), None, None, None),
            (None, Some(110_i64), None, None),
            (None, None, Some(1_i64), None),
            (None, None, None, Some(110_i64)),
        ] {
            sqlx::query("INSERT INTO platform_organization_policies (organization_id,requests_per_minute,tokens_per_minute,concurrent_requests,monthly_budget_microusd) VALUES ($1,$2,$3,$4,$5) ON CONFLICT (organization_id) DO UPDATE SET requests_per_minute=$2,tokens_per_minute=$3,concurrent_requests=$4,monthly_budget_microusd=$5")
                .bind(f.principal.organization_id).bind(requests).bind(tokens).bind(concurrency).bind(budget)
                .execute(&f.store.pool).await.unwrap();
            assert_eq!(
                admit(&f.store, &f.start(), &request(), 60).await,
                Err(InferenceError::Busy)
            );
        }
    }

    #[sqlx::test]
    async fn cached_target_must_match_price_admission_snapshot(pool: PgPool) {
        use crate::inference::repository::InferenceRepository;
        let f = fixture(pool).await;
        sqlx::query("UPDATE deployments SET enabled=true")
            .execute(&f.store.pool)
            .await
            .unwrap();
        sqlx::query("UPDATE provider_connections SET enabled=true")
            .execute(&f.store.pool)
            .await
            .unwrap();
        let cached = f
            .store
            .deployments(&f.principal, "company/smart")
            .await
            .unwrap()
            .remove(0);
        sqlx::query("UPDATE deployments SET upstream_model='replacement'")
            .execute(&f.store.pool)
            .await
            .unwrap();
        f.price(1_000_000).await;
        let start = f.start();
        assert_eq!(
            admit_for_deployment(&f.store, &start, &request(), 30, &cached).await,
            Err(InferenceError::Configuration)
        );
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM governance_reservations")
            .fetch_one(&f.store.pool)
            .await
            .unwrap();
        assert_eq!(count, 0);
        let current = f
            .store
            .deployments(&f.principal, "company/smart")
            .await
            .unwrap()
            .remove(0);
        admit_for_deployment(&f.store, &start, &request(), 30, &current)
            .await
            .unwrap();
    }
    #[sqlx::test]
    async fn reconciliation_rechecks_revoked_operator_after_waiting_for_lock(pool: PgPool) {
        let f = fixture(pool).await;
        f.price(1_000_000).await;
        let start = f.start();
        admit(&f.store, &start, &request(), 30).await.unwrap();
        finish(&f.store, &done(start.id, None, None)).await.unwrap();
        let mut tx = f.store.pool.begin().await.unwrap();
        sqlx::query("SELECT id FROM organizations WHERE id=$1 FOR NO KEY UPDATE")
            .bind(f.principal.organization_id)
            .execute(&mut *tx)
            .await
            .unwrap();
        sqlx::query("UPDATE users SET platform_admin=false WHERE id=$1")
            .bind(f.principal.user_id)
            .execute(&mut *tx)
            .await
            .unwrap();
        let op = resolve_usage(
            &f.store,
            f.principal.organization_id,
            start.id,
            Usage {
                input_tokens: Some(1),
                output_tokens: Some(1),
            },
            "receipt:123",
            f.principal.user_id.unwrap(),
        );
        tokio::pin!(op);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), &mut op)
                .await
                .is_err()
        );
        tx.commit().await.unwrap();
        assert_eq!(op.await, Err(InferenceError::InvalidRequest));
        assert_eq!(amounts(&f, start.id).await.0, "unknown");
    }
    #[sqlx::test]
    async fn compaction_preserves_unknown_reservations_and_immutable_ledger(pool: PgPool) {
        let f = fixture(pool).await;
        f.price(1_000_000).await;
        let known = f.start();
        let unknown = f.start();
        admit(&f.store, &known, &request(), 30).await.unwrap();
        finish(&f.store, &done(known.id, Some(2), Some(3)))
            .await
            .unwrap();
        admit(&f.store, &unknown, &request(), 30).await.unwrap();
        finish(&f.store, &done(unknown.id, None, None))
            .await
            .unwrap();
        sqlx::query("UPDATE inference_executions SET completed_at=now()-interval '90 days'")
            .execute(&f.store.pool)
            .await
            .unwrap();
        let before: i64 = sqlx::query_scalar("SELECT count(*) FROM monetary_ledger")
            .fetch_one(&f.store.pool)
            .await
            .unwrap();
        assert_eq!(
            crate::maintenance::compact_history(&f.store, 30, 100)
                .await
                .unwrap(),
            1
        );
        let rows: Vec<(Uuid, bool)> =
            sqlx::query_as("SELECT id,details_redacted_at IS NOT NULL FROM inference_executions")
                .fetch_all(&f.store.pool)
                .await
                .unwrap();
        assert!(rows.contains(&(known.id, true)));
        assert!(rows.contains(&(unknown.id, false)));
        let after: i64 = sqlx::query_scalar("SELECT count(*) FROM monetary_ledger")
            .fetch_one(&f.store.pool)
            .await
            .unwrap();
        assert_eq!(before, after);
        assert_eq!(amounts(&f, known.id).await.2, Some(5));
    }
    #[sqlx::test]
    async fn concurrent_admission_is_atomic_across_transactions(pool: PgPool) {
        let f = fixture(pool).await;
        f.price(1_000_000).await;
        f.policy("organization", Some(100), Some(110), Some(1), Some(110))
            .await;
        let a = f.start();
        let b = f.start();
        let req = request();
        let (a, b) = tokio::join!(admit(&f.store, &a, &req, 60), admit(&f.store, &b, &req, 60));
        assert!(matches!(
            (a, b),
            (Ok(()), Err(InferenceError::Busy)) | (Err(InferenceError::Busy), Ok(()))
        ));
        let n: i64 = sqlx::query_scalar("SELECT count(*) FROM inference_executions")
            .fetch_one(&f.store.pool)
            .await
            .unwrap();
        assert_eq!(n, 1);
    }

    #[sqlx::test(migrations = false)]
    async fn legacy_backfill_preserves_unknown_cost_and_pending_completion(pool: PgPool) {
        for migration in [
            include_str!("../../migrations/0001_foundation.sql"),
            include_str!("../../migrations/0002_inference_engine.sql"),
            include_str!("../../migrations/0003_control_plane.sql"),
        ] {
            sqlx::raw_sql(migration).execute(&pool).await.unwrap();
        }
        let f = fixture(pool).await;
        let old = Uuid::new_v4();
        sqlx::query("INSERT INTO inference_executions(id,organization_id,workspace_id,api_key_id,deployment_id,public_model,provider,streamed,state) VALUES($1,$2,$3,$4,$5,'old','openai',false,'started')")
            .bind(old).bind(f.principal.organization_id).bind(f.principal.workspace_id).bind(f.principal.key_id).bind(f.deployment).execute(&f.store.pool).await.unwrap();
        sqlx::raw_sql(include_str!("../../migrations/0004_governance.sql"))
            .execute(&f.store.pool)
            .await
            .unwrap();
        assert_eq!(amounts(&f, old).await, ("pending".into(), None, None));
        finish(&f.store, &done(old, Some(0), Some(0)))
            .await
            .unwrap();
        assert_eq!(amounts(&f, old).await, ("unknown".into(), None, None));
        let n: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM monetary_ledger WHERE execution_id=$1 AND kind='unknown'",
        )
        .bind(old)
        .fetch_one(&f.store.pool)
        .await
        .unwrap();
        assert_eq!(n, 1);
        for migration in [
            include_str!("../../migrations/0005_routing.sql"),
            include_str!("../../migrations/0006_execution_retention.sql"),
            include_str!("../../migrations/0007_platform_catalog.sql"),
        ] {
            sqlx::raw_sql(migration)
                .execute(&f.store.pool)
                .await
                .unwrap();
        }
        f.price(1_000_000).await;
        f.policy("organization", None, Some(1000), None, None).await;
        assert_eq!(
            admit(&f.store, &f.start(), &request(), 60).await,
            Err(InferenceError::Busy)
        );
        sqlx::query(
            "UPDATE governance_policies SET tokens_per_minute=NULL,monthly_budget_microusd=1000",
        )
        .execute(&f.store.pool)
        .await
        .unwrap();
        assert_eq!(
            admit(&f.store, &f.start(), &request(), 60).await,
            Err(InferenceError::Busy)
        );
    }

    #[sqlx::test]
    async fn oversized_accounting_fails_closed_without_losing_hold(pool: PgPool) {
        let f = fixture(pool).await;
        f.price(1_000_000).await;
        let start = f.start();
        admit(&f.store, &start, &request(), 60).await.unwrap();
        assert_eq!(
            finish(&f.store, &done(start.id, Some(i64::MAX as u64), Some(1))).await,
            Err(InferenceError::Storage)
        );
        assert_eq!(
            amounts(&f, start.id).await,
            ("pending".into(), Some(110), None)
        );
        sqlx::query("INSERT INTO deployment_prices(id,organization_id,deployment_id,input_microusd_per_million,output_microusd_per_million,input_token_limit,output_token_limit) VALUES($1,$2,$3,1,1,$4,50)")
            .bind(Uuid::new_v4()).bind(f.principal.organization_id).bind(f.deployment).bind(i64::MAX).execute(&f.store.pool).await.unwrap();
        assert_eq!(
            admit(&f.store, &f.start(), &request(), 60).await,
            Err(InferenceError::Configuration)
        );
        let n: i64 = sqlx::query_scalar("SELECT count(*) FROM inference_executions")
            .fetch_one(&f.store.pool)
            .await
            .unwrap();
        assert_eq!(n, 1);
    }

    #[sqlx::test]
    async fn partial_usage_grows_hold_and_resolution_is_transactional(pool: PgPool) {
        let f = fixture(pool).await;
        f.price(1_000_000).await;
        let start = f.start();
        admit(&f.store, &start, &request(), 60).await.unwrap();
        finish(&f.store, &done(start.id, Some(300), None))
            .await
            .unwrap();
        assert_eq!(
            amounts(&f, start.id).await,
            ("unknown".into(), Some(300), None)
        );
        let usage = Usage {
            input_tokens: Some(300),
            output_tokens: Some(2),
        };
        // An invalid actor causes audit FK failure: settlement and ledger roll back.
        assert_eq!(
            resolve_usage(
                &f.store,
                f.principal.organization_id,
                start.id,
                usage,
                "invoice:1",
                Uuid::new_v4()
            )
            .await,
            Err(InferenceError::InvalidRequest)
        );
        assert_eq!(
            amounts(&f, start.id).await,
            ("unknown".into(), Some(300), None)
        );
        let n: i64 =
            sqlx::query_scalar("SELECT count(*) FROM monetary_ledger WHERE kind='reconciliation'")
                .fetch_one(&f.store.pool)
                .await
                .unwrap();
        assert_eq!(n, 0);
    }

    #[sqlx::test]
    async fn each_limit_independently_rejects_a_racing_attempt(pool: PgPool) {
        let f = fixture(pool).await;
        f.price(1_000_000).await;
        for (requests, tokens, concurrency, budget) in [
            (Some(1), None, None, None),
            (None, Some(110), None, None),
            (None, None, Some(1), None),
            (None, None, None, Some(110)),
        ] {
            // Isolated scope using a fresh workspace/key for each limiter.
            let workspace = Uuid::new_v4();
            let key = Uuid::new_v4();
            sqlx::query("INSERT INTO workspaces(id,organization_id,name,kind) VALUES($1,$2,'isolated','team')").bind(workspace).bind(f.principal.organization_id).execute(&f.store.pool).await.unwrap();
            sqlx::query("INSERT INTO api_keys(id,organization_id,workspace_id,issued_to_user_id,name,secret_hash) VALUES($1,$2,$3,$4,'isolated',decode(repeat('00',32),'hex'))").bind(key).bind(f.principal.organization_id).bind(workspace).bind(f.principal.user_id).execute(&f.store.pool).await.unwrap();
            sqlx::query("INSERT INTO workspace_memberships (organization_id,workspace_id,user_id,role) VALUES ($1,$2,$3,'member')")
                .bind(f.principal.organization_id).bind(workspace).bind(f.principal.user_id).execute(&f.store.pool).await.unwrap();
            sqlx::query("INSERT INTO workspace_model_grants (organization_id,workspace_id,model_id) SELECT $1,$2,model_id FROM deployments WHERE id=$3")
                .bind(f.principal.organization_id).bind(workspace).bind(f.deployment).execute(&f.store.pool).await.unwrap();
            let isolated = Fixture {
                store: f.store.clone(),
                principal: Principal {
                    workspace_id: workspace,
                    key_id: key,
                    ..f.principal
                },
                team: f.team,
                deployment: f.deployment,
            };
            isolated
                .policy("workspace", requests, tokens, concurrency, budget)
                .await;
            let a = isolated.start();
            let b = isolated.start();
            let req = request();
            let (a, b) = tokio::join!(
                admit(&isolated.store, &a, &req, 60),
                admit(&isolated.store, &b, &req, 60)
            );
            assert!(matches!(
                (a, b),
                (Ok(()), Err(InferenceError::Busy)) | (Err(InferenceError::Busy), Ok(()))
            ));
        }
    }

    #[sqlx::test]
    async fn scopes_and_tenant_constraints(pool: PgPool) {
        let f = fixture(pool).await;
        f.policy("key", Some(1), None, None, None).await;
        let first = f.start();
        admit(&f.store, &first, &request(), 60).await.unwrap();
        assert_eq!(
            admit(&f.store, &f.start(), &request(), 60).await,
            Err(InferenceError::Busy)
        );
        let mut other = f.start();
        other.principal = f.team;
        admit(&f.store, &other, &request(), 60).await.unwrap();
        let wrong_org = Uuid::new_v4();
        sqlx::query("INSERT INTO organizations(id,slug,name) VALUES($1,'other','other')")
            .bind(wrong_org)
            .execute(&f.store.pool)
            .await
            .unwrap();
        assert!(sqlx::query("INSERT INTO governance_policies(id,organization_id,scope,workspace_id,api_key_id) VALUES($1,$2,'key',$3,$4)")
            .bind(Uuid::new_v4()).bind(wrong_org).bind(f.principal.workspace_id).bind(f.principal.key_id).execute(&f.store.pool).await.is_err());
        assert!(sqlx::query("INSERT INTO governance_policies(id,organization_id,scope,api_key_id) VALUES($1,$2,'organization',$3)")
            .bind(Uuid::new_v4()).bind(f.principal.organization_id).bind(f.principal.key_id).execute(&f.store.pool).await.is_err());
    }

    #[sqlx::test]
    async fn price_pinning_idempotent_settlement_and_immutability(pool: PgPool) {
        let f = fixture(pool).await;
        let price = f.price(1_000_000).await;
        let start = f.start();
        admit(&f.store, &start, &request(), 60).await.unwrap();
        f.price(2_000_000).await;
        finish(&f.store, &done(start.id, Some(3), Some(2)))
            .await
            .unwrap();
        finish(&f.store, &done(start.id, Some(3), Some(2)))
            .await
            .unwrap();
        assert_eq!(
            amounts(&f, start.id).await,
            ("settled".into(), Some(110), Some(5))
        );
        assert_eq!(
            finish(&f.store, &done(start.id, Some(4), Some(2))).await,
            Err(InferenceError::Storage)
        );
        let n: i64 =
            sqlx::query_scalar("SELECT count(*) FROM monetary_ledger WHERE execution_id=$1")
                .bind(start.id)
                .fetch_one(&f.store.pool)
                .await
                .unwrap();
        assert_eq!(n, 2);
        assert!(
            sqlx::query("UPDATE deployment_prices SET input_token_limit=200 WHERE id=$1")
                .bind(price)
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
    }

    #[sqlx::test]
    async fn missing_usage_holds_manual_resolution_and_audit(pool: PgPool) {
        let f = fixture(pool).await;
        f.price(1_000_000).await;
        f.policy("organization", None, None, None, Some(110)).await;
        let start = f.start();
        admit(&f.store, &start, &request(), 60).await.unwrap();
        finish(&f.store, &done(start.id, Some(3), None))
            .await
            .unwrap();
        assert_eq!(
            amounts(&f, start.id).await,
            ("unknown".into(), Some(110), None)
        );
        assert_eq!(
            admit(&f.store, &f.start(), &request(), 60).await,
            Err(InferenceError::Busy)
        );
        let actor = f.principal.user_id.unwrap();
        let usage = Usage {
            input_tokens: Some(3),
            output_tokens: Some(2),
        };
        assert_eq!(
            resolve_usage(
                &f.store,
                Uuid::new_v4(),
                start.id,
                usage,
                "invoice:123",
                actor
            )
            .await,
            Err(InferenceError::Storage)
        );
        assert_eq!(
            resolve_usage(
                &f.store,
                f.principal.organization_id,
                start.id,
                Usage {
                    input_tokens: Some(2),
                    output_tokens: Some(2)
                },
                "invoice:123",
                actor
            )
            .await,
            Err(InferenceError::InvalidRequest)
        );
        resolve_usage(
            &f.store,
            f.principal.organization_id,
            start.id,
            usage,
            "invoice:123",
            actor,
        )
        .await
        .unwrap();
        resolve_usage(
            &f.store,
            f.principal.organization_id,
            start.id,
            usage,
            "invoice:123",
            actor,
        )
        .await
        .unwrap();
        let n: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM audit_events WHERE action='usage.reconciled' AND target_id=$1",
        )
        .bind(start.id)
        .fetch_one(&f.store.pool)
        .await
        .unwrap();
        assert_eq!(n, 1);
        assert_eq!(
            amounts(&f, start.id).await,
            ("settled".into(), Some(110), Some(5))
        );
    }

    #[sqlx::test]
    async fn stale_leases_release_concurrency_not_money(pool: PgPool) {
        let f = fixture(pool).await;
        f.price(1_000_000).await;
        f.policy("organization", None, None, Some(1), None).await;
        let first = f.start();
        admit(&f.store, &first, &request(), 60).await.unwrap();
        sqlx::query("UPDATE governance_reservations SET lease_expires_at=clock_timestamp()-interval '1 second'").execute(&f.store.pool).await.unwrap();
        // Admission ignores expired leases even before reconciliation has run.
        admit(&f.store, &f.start(), &request(), 60).await.unwrap();
        assert_eq!(reconcile_expired(&f.store, 100).await.unwrap(), 1);
        assert_eq!(reconcile_expired(&f.store, 100).await.unwrap(), 0);
        assert_eq!(
            amounts(&f, first.id).await,
            ("unknown".into(), Some(110), None)
        );
        let state: (String, String) =
            sqlx::query_as("SELECT state,error_code FROM inference_executions WHERE id=$1")
                .bind(first.id)
                .fetch_one(&f.store.pool)
                .await
                .unwrap();
        assert_eq!(state, ("cancelled".into(), "lease_expired".into()));
        assert_eq!(
            finish(&f.store, &done(first.id, Some(0), Some(0))).await,
            Err(InferenceError::Storage)
        );
    }

    #[sqlx::test]
    async fn unbounded_requests_and_unknown_historical_usage_fail_closed(pool: PgPool) {
        let f = fixture(pool).await;
        let mut req = request();
        req.max_output_tokens = None;
        let unpriced = f.start();
        admit(&f.store, &unpriced, &req, 60).await.unwrap();
        finish(&f.store, &done(unpriced.id, Some(0), Some(0)))
            .await
            .unwrap();
        assert_eq!(
            amounts(&f, unpriced.id).await,
            ("unknown".into(), None, None)
        );
        f.policy("organization", None, Some(1000), None, Some(1000))
            .await;
        assert_eq!(
            admit(&f.store, &f.start(), &request(), 60).await,
            Err(InferenceError::Configuration)
        );
        f.price(1_000_000).await;
        assert_eq!(
            admit(&f.store, &f.start(), &req, 60).await,
            Err(InferenceError::Configuration)
        );
        assert_eq!(
            admit(&f.store, &f.start(), &request(), 60).await,
            Err(InferenceError::Busy)
        );
    }

    #[sqlx::test]
    async fn provider_failure_keeps_hold_and_actual_overspend_blocks_future(pool: PgPool) {
        let f = fixture(pool).await;
        f.price(1_000_000).await;
        f.policy("organization", None, None, None, Some(220)).await;
        let a = f.start();
        admit(&f.store, &a, &request(), 60).await.unwrap();
        let mut failed = done(a.id, Some(0), Some(0));
        failed.outcome = Outcome::Failed;
        failed.error = Some(InferenceError::UpstreamRejected);
        finish(&f.store, &failed).await.unwrap();
        assert_eq!(amounts(&f, a.id).await, ("unknown".into(), Some(110), None));
        let b = f.start();
        admit(&f.store, &b, &request(), 60).await.unwrap();
        finish(&f.store, &done(b.id, Some(300), Some(10)))
            .await
            .unwrap();
        assert_eq!(
            amounts(&f, b.id).await,
            ("settled".into(), Some(110), Some(310))
        );
        assert_eq!(
            admit(&f.store, &f.start(), &request(), 60).await,
            Err(InferenceError::Busy)
        );
    }

    #[sqlx::test]
    async fn utc_boundaries_and_admission_month_survive_late_settlement(pool: PgPool) {
        let f = fixture(pool).await;
        // Independent of session timezone, including DST and year/month rollover.
        let starts: (String, String) = sqlx::query_as("SELECT to_char(date_trunc('month','2025-01-01 00:00:01+00'::timestamptz,'UTC') AT TIME ZONE 'UTC','YYYY-MM-DD HH24:MI:SS'),to_char(date_trunc('minute','2024-12-31 23:59:59+00'::timestamptz,'UTC') AT TIME ZONE 'UTC','YYYY-MM-DD HH24:MI:SS')")
            .fetch_one(&f.store.pool).await.unwrap();
        assert_eq!(
            starts,
            ("2025-01-01 00:00:00".into(), "2024-12-31 23:59:00".into())
        );
        f.price(1_000_000).await;
        f.policy("organization", Some(1), None, None, Some(110))
            .await;
        let a = f.start();
        admit(&f.store, &a, &request(), 60).await.unwrap();
        // Simulate an attempt admitted before rollover, without process-clock sleeps.
        sqlx::query("UPDATE governance_reservations SET minute_start=date_trunc('minute',clock_timestamp(),'UTC')-interval '1 minute',month_start=date_trunc('month',clock_timestamp(),'UTC')-interval '1 month' WHERE execution_id=$1")
            .bind(a.id).execute(&f.store.pool).await.unwrap();
        admit(&f.store, &f.start(), &request(), 60).await.unwrap();
        finish(&f.store, &done(a.id, Some(500), Some(1)))
            .await
            .unwrap();
        let prior: bool = sqlx::query_scalar("SELECT month_start < date_trunc('month',clock_timestamp(),'UTC') FROM governance_reservations WHERE execution_id=$1")
            .bind(a.id).fetch_one(&f.store.pool).await.unwrap();
        assert!(prior);
    }
}
