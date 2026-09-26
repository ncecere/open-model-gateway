use super::*;

fn candidate(id: u128, priority: i32, weight: i32, residency: &str) -> Candidate {
    Candidate {
        deployment_id: Uuid::from_u128(id),
        priority,
        weight,
        residency: residency.into(),
        operator_disabled: false,
        circuit_open: false,
    }
}

#[test]
fn defaults_preserve_first_available_and_never_retry() {
    let policy = RoutingPolicy::default();
    let result = order_candidates(
        &policy,
        vec![
            candidate(3, 0, 1, "unspecified"),
            candidate(1, 0, 900, "us"),
        ],
        [0; 32],
    )
    .unwrap();
    assert_eq!(
        result,
        RoutePlan::single(vec![Uuid::from_u128(3), Uuid::from_u128(1)])
    );
    assert!(!may_failover(&result, InferenceError::Busy));
}

#[test]
fn priority_is_stable_and_lower_values_win() {
    let result = order_candidates(
        &RoutingPolicy::default(),
        vec![
            candidate(1, 10, 1000, "us"),
            candidate(2, -10, 1, "us"),
            candidate(3, -10, 1, "eu"),
        ],
        [0; 32],
    )
    .unwrap();
    assert_eq!(
        result.deployment_ids,
        vec![Uuid::from_u128(2), Uuid::from_u128(3), Uuid::from_u128(1)]
    );
}

#[test]
fn required_residency_fences_primary_even_when_preferred_route_is_cooling_down() {
    let policy = RoutingPolicy {
        required_residency: Some("us".into()),
        ..RoutingPolicy::default()
    };
    let mut first = candidate(1, 0, 1, "us");
    first.circuit_open = true;
    let plan = order_candidates(
        &policy,
        vec![first, candidate(2, 1, 1, "eu"), candidate(3, 2, 1, "us")],
        [0; 32],
    )
    .unwrap();
    assert_eq!(plan.deployment_ids, vec![Uuid::from_u128(3)]);
}
#[test]
fn weighted_is_without_replacement_and_never_crosses_priority_tiers() {
    let policy = RoutingPolicy {
        strategy: "weighted".into(),
        ..RoutingPolicy::default()
    };
    let input = vec![
        candidate(1, 0, 1, "us"),
        candidate(2, 0, 9, "us"),
        candidate(3, 1, 1000, "us"),
    ];
    let mut heavy_first = 0;
    for seed in 0..1000u32 {
        let mut bytes = [0; 32];
        bytes[..4].copy_from_slice(&seed.to_le_bytes());
        let plan = order_candidates(&policy, input.clone(), bytes).unwrap();
        assert_eq!(
            plan,
            order_candidates(&policy, input.clone(), bytes).unwrap()
        );
        assert_eq!(plan.deployment_ids.len(), 3);
        assert_eq!(plan.deployment_ids.iter().collect::<HashSet<_>>().len(), 3);
        assert_eq!(plan.deployment_ids[2], Uuid::from_u128(3));
        heavy_first += usize::from(plan.deployment_ids[0] == Uuid::from_u128(2));
    }
    assert!(
        (830..970).contains(&heavy_first),
        "weighted distribution: {heavy_first}"
    );
}

#[test]
fn disabled_and_open_routes_are_skipped_without_probes() {
    let mut disabled = candidate(1, -1, 1, "us");
    disabled.operator_disabled = true;
    let mut open = candidate(2, 0, 1, "us");
    open.circuit_open = true;
    assert_eq!(
        order_candidates(
            &RoutingPolicy::default(),
            vec![disabled.clone(), open.clone(), candidate(3, 1, 1, "us")],
            [0; 32]
        )
        .unwrap()
        .deployment_ids,
        vec![Uuid::from_u128(3)]
    );
    assert_eq!(
        order_candidates(&RoutingPolicy::default(), vec![disabled, open], [0; 32]),
        Err(InferenceError::ModelUnavailable)
    );
}

#[test]
fn retries_require_exact_explicit_residency_of_first_selected_route() {
    let policy = RoutingPolicy {
        max_attempts: 3,
        ..RoutingPolicy::default()
    };
    let plan = order_candidates(
        &policy,
        vec![
            candidate(1, 0, 1, "us"),
            candidate(2, 1, 1, "unspecified"),
            candidate(3, 2, 1, "eu"),
            candidate(4, 3, 1, "us"),
        ],
        [0; 32],
    )
    .unwrap();
    assert_eq!(
        plan.deployment_ids,
        vec![Uuid::from_u128(1), Uuid::from_u128(4)]
    );
    let plan = order_candidates(
        &policy,
        vec![
            candidate(1, 0, 1, "unspecified"),
            candidate(2, 0, 1, "unspecified"),
            candidate(3, 0, 1, "us"),
        ],
        [0; 32],
    )
    .unwrap();
    assert_eq!(plan.deployment_ids, vec![Uuid::from_u128(1)]);
}

#[test]
fn failover_is_explicit_and_transport_ambiguity_is_separate_opt_in() {
    let mut plan = RoutePlan {
        max_attempts: 3,
        ..RoutePlan::default()
    };
    assert!(may_failover(&plan, InferenceError::Busy));
    assert!(!may_failover(&plan, InferenceError::UpstreamUnavailable));
    plan.allow_ambiguous_failover = true;
    assert!(may_failover(&plan, InferenceError::UpstreamUnavailable));
    for error in [
        InferenceError::InvalidRequest,
        InferenceError::ModelUnavailable,
        InferenceError::Unsupported,
        InferenceError::Configuration,
        InferenceError::Timeout,
        InferenceError::UpstreamRejected,
        InferenceError::InvalidUpstream,
        InferenceError::Storage,
    ] {
        assert!(!may_failover(&plan, error));
        assert_eq!(
            affects_health(Some(error)),
            error == InferenceError::Timeout
        );
    }
    assert!(affects_health(None));
    assert!(affects_health(Some(InferenceError::Busy)));
    assert!(affects_health(Some(InferenceError::UpstreamUnavailable)));
}

#[test]
fn invalid_config_and_oversized_candidate_lists_are_rejected() {
    assert_eq!(
        validate_ids(&vec![Uuid::nil(); MAX_CANDIDATES + 1]),
        Err(InferenceError::Configuration)
    );
    assert_eq!(
        validate_ids(&[Uuid::nil(), Uuid::nil()]),
        Err(InferenceError::Configuration)
    );
    for policy in [
        RoutingPolicy {
            strategy: "random".into(),
            ..RoutingPolicy::default()
        },
        RoutingPolicy {
            max_attempts: 0,
            ..RoutingPolicy::default()
        },
        RoutingPolicy {
            max_attempts: 4,
            ..RoutingPolicy::default()
        },
        RoutingPolicy {
            failure_threshold: 0,
            ..RoutingPolicy::default()
        },
        RoutingPolicy {
            cooldown_seconds: 0,
            ..RoutingPolicy::default()
        },
        RoutingPolicy {
            cooldown_seconds: 3601,
            ..RoutingPolicy::default()
        },
    ] {
        assert_eq!(policy.validate(), Err(InferenceError::Configuration));
    }
    for residency in ["", "US", " us", "us ", "unspecified ", "us/east"] {
        assert!(!valid_residency(residency));
    }
    assert!(!valid_residency(&"a".repeat(65)));
    assert!(valid_residency("us-east_1.example"));
    assert_eq!(
        order_candidates(
            &RoutingPolicy::default(),
            vec![candidate(1, 0, 0, "us")],
            [0; 32]
        ),
        Err(InferenceError::Configuration)
    );
}

#[cfg(feature = "integration-tests")]
mod database {
    use super::*;
    use sqlx::PgPool;

    async fn fixture(pool: PgPool) -> (Store, Principal, Uuid, Deployment) {
        let org = Uuid::new_v4();
        let model = Uuid::new_v4();
        let connection = Uuid::new_v4();
        let deployment = Uuid::new_v4();
        sqlx::query("INSERT INTO organizations (id,slug,name) VALUES ($1,$2,'routing test')")
            .bind(org)
            .bind(org.to_string())
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO models (id,organization_id,public_name,display_name,enabled) VALUES ($1,$2,'test','test',true)")
            .bind(model).bind(org).execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO provider_connections (id,organization_id,name,provider,credential_ref,enabled) VALUES ($1,$2,'test','mock','env:NOT_RESOLVED',true)")
            .bind(connection).bind(org).execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO deployments (id,organization_id,model_id,provider_connection_id,upstream_model,enabled) VALUES ($1,$2,$3,$4,'test',true)")
            .bind(deployment).bind(org).bind(model).bind(connection).execute(&pool).await.unwrap();
        let principal = Principal {
            organization_id: org,
            workspace_id: Uuid::new_v4(),
            key_id: Uuid::new_v4(),
            user_id: None,
        };
        let deployment = Deployment {
            id: deployment,
            provider: "mock".into(),
            upstream_model: "test".into(),
            credential_ref: "env:NOT_RESOLVED".into(),
            endpoint: None,
            region: None,
        };
        (Store::new(pool), principal, model, deployment)
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn circuit_opens_expires_and_success_resets(pool: PgPool) {
        let (store, principal, model, deployment) = fixture(pool).await;
        let org = principal.organization_id;
        sqlx::query("INSERT INTO model_routing_policies (organization_id,model_id,failure_threshold,cooldown_seconds) VALUES ($1,$2,2,60)")
            .bind(org).bind(model).execute(&store.pool).await.unwrap();
        assert!(
            health(&store, org, deployment.id)
                .await
                .unwrap()
                .last_observed_at
                .is_none()
        );
        record_result(&store, org, deployment.id, Some(InferenceError::Busy))
            .await
            .unwrap();
        assert!(
            !health(&store, org, deployment.id)
                .await
                .unwrap()
                .circuit_open
        );
        record_result(&store, org, deployment.id, Some(InferenceError::Timeout))
            .await
            .unwrap();
        let observed = health(&store, org, deployment.id).await.unwrap();
        assert!(observed.circuit_open);
        assert_eq!(observed.consecutive_failures, 2);
        assert_eq!(
            plan(
                &store,
                &principal,
                "test",
                std::slice::from_ref(&deployment),
                Uuid::new_v4()
            )
            .await,
            Err(InferenceError::ModelUnavailable)
        );
        record_result(&store, org, deployment.id, Some(InferenceError::Storage))
            .await
            .unwrap();
        assert_eq!(
            health(&store, org, deployment.id)
                .await
                .unwrap()
                .consecutive_failures,
            2
        );
        sqlx::query("UPDATE deployment_route_health SET open_until=now()-interval '1 second' WHERE deployment_id=$1")
            .bind(deployment.id).execute(&store.pool).await.unwrap();
        assert!(
            plan(
                &store,
                &principal,
                "test",
                std::slice::from_ref(&deployment),
                Uuid::new_v4()
            )
            .await
            .is_ok()
        );
        record_result(
            &store,
            org,
            deployment.id,
            Some(InferenceError::UpstreamUnavailable),
        )
        .await
        .unwrap();
        assert!(
            health(&store, org, deployment.id)
                .await
                .unwrap()
                .circuit_open
        );
        record_result(&store, org, deployment.id, None)
            .await
            .unwrap();
        let observed = health(&store, org, deployment.id).await.unwrap();
        assert_eq!(observed.consecutive_failures, 0);
        assert!(observed.open_until.is_none());
        assert!(observed.last_observed_at.is_some());
        sqlx::query("INSERT INTO deployment_routing (organization_id,deployment_id,operator_disabled) VALUES ($1,$2,true)")
            .bind(org).bind(deployment.id).execute(&store.pool).await.unwrap();
        assert_eq!(
            plan(&store, &principal, "test", &[deployment], Uuid::new_v4()).await,
            Err(InferenceError::ModelUnavailable)
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn concurrent_failures_are_atomic_and_health_is_global(pool: PgPool) {
        let (store, principal, model, deployment) = fixture(pool).await;
        let org = principal.organization_id;
        let results = futures_util::future::join_all(
            (0..24).map(|_| record_result(&store, org, deployment.id, Some(InferenceError::Busy))),
        )
        .await;
        assert!(results.iter().all(Result::is_ok));
        assert_eq!(
            health(&store, org, deployment.id)
                .await
                .unwrap()
                .consecutive_failures,
            24
        );
        let other = Uuid::new_v4();
        sqlx::query("INSERT INTO organizations (id,slug,name) VALUES ($1,$2,'other')")
            .bind(other)
            .bind(other.to_string())
            .execute(&store.pool)
            .await
            .unwrap();
        // Passive health is shared by resource ID, never scoped by legacy provenance.
        record_result(&store, other, deployment.id, None)
            .await
            .unwrap();
        assert_eq!(
            health(&store, org, deployment.id)
                .await
                .unwrap()
                .consecutive_failures,
            0
        );
        assert_eq!(
            health(&store, other, deployment.id)
                .await
                .unwrap()
                .consecutive_failures,
            0
        );
        sqlx::query("INSERT INTO model_routing_policies (model_id,max_attempts) VALUES ($1,2)")
            .bind(model)
            .execute(&store.pool)
            .await
            .unwrap();
        let other_principal = Principal {
            organization_id: other,
            ..principal
        };
        assert_eq!(
            plan(
                &store,
                &other_principal,
                "test",
                std::slice::from_ref(&deployment),
                Uuid::new_v4()
            )
            .await,
            Err(InferenceError::ModelUnavailable)
        );
        sqlx::query("INSERT INTO organization_model_grants (organization_id,model_id,public_name) VALUES ($1,$2,'other-alias')")
            .bind(other).bind(model).execute(&store.pool).await.unwrap();
        let shared = plan(
            &store,
            &other_principal,
            "other-alias",
            &[deployment],
            Uuid::new_v4(),
        )
        .await
        .unwrap();
        assert_eq!(shared.max_attempts, 2);
    }
}
