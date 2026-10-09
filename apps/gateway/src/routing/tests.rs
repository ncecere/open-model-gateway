use super::*;
fn candidate(id: u128, priority: i32, weight: i32, residency: &str) -> Candidate {
    Candidate {
        deployment_id: Uuid::from_u128(id),
        priority,
        weight,
        residency: residency.into(),
        operator_disabled: false,
        circuit_open: false,
        cooldown_remaining_seconds: 0,
    }
}
#[test]
fn defaults_preserve_first_available_and_never_retry() {
    let p = RoutingPolicy::default();
    let plan = order_candidates(
        &p,
        vec![
            candidate(3, 0, 1, "unspecified"),
            candidate(1, 0, 900, "us"),
        ],
        [0; 32],
    )
    .unwrap();
    assert_eq!(
        plan,
        RoutePlan::single(vec![Uuid::from_u128(3), Uuid::from_u128(1)])
    );
    assert!(!may_failover(&plan, InferenceError::Busy));
}
#[test]
fn priority_is_stable_and_lower_values_win() {
    let plan = order_candidates(
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
        plan.deployment_ids,
        vec![Uuid::from_u128(2), Uuid::from_u128(3), Uuid::from_u128(1)]
    );
}
#[test]
fn required_residency_fences_primary_even_when_preferred_route_is_cooling_down() {
    let p = RoutingPolicy {
        required_residency: Some("us".into()),
        ..Default::default()
    };
    let mut first = candidate(1, 0, 1, "us");
    first.circuit_open = true;
    assert_eq!(
        order_candidates(
            &p,
            vec![first, candidate(2, 1, 1, "eu"), candidate(3, 2, 1, "us")],
            [0; 32]
        )
        .unwrap()
        .deployment_ids,
        vec![Uuid::from_u128(3)]
    );
}
#[test]
fn weighted_is_without_replacement_and_never_crosses_priority_tiers() {
    let p = RoutingPolicy {
        strategy: "weighted".into(),
        ..Default::default()
    };
    let input = vec![
        candidate(1, 0, 1, "us"),
        candidate(2, 0, 9, "us"),
        candidate(3, 1, 1000, "us"),
    ];
    let mut heavy = 0;
    for seed in 0..1000u32 {
        let mut bytes = [0; 32];
        bytes[..4].copy_from_slice(&seed.to_le_bytes());
        let plan = order_candidates(&p, input.clone(), bytes).unwrap();
        assert_eq!(plan, order_candidates(&p, input.clone(), bytes).unwrap());
        assert_eq!(plan.deployment_ids.len(), 3);
        assert_eq!(plan.deployment_ids.iter().collect::<HashSet<_>>().len(), 3);
        assert_eq!(plan.deployment_ids[2], Uuid::from_u128(3));
        heavy += usize::from(plan.deployment_ids[0] == Uuid::from_u128(2));
    }
    assert!((830..970).contains(&heavy));
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
    // Only cooling-down routes are left: retryable, never "model not found".
    open.cooldown_remaining_seconds = 12;
    let mut later = candidate(4, 0, 1, "us");
    later.circuit_open = true;
    later.cooldown_remaining_seconds = 25;
    assert_eq!(
        order_candidates(
            &RoutingPolicy::default(),
            vec![disabled.clone(), later, open],
            [0; 32]
        ),
        Err(InferenceError::RouteCoolingDown(12))
    );
    assert_eq!(
        order_candidates(&RoutingPolicy::default(), vec![disabled], [0; 32]),
        Err(InferenceError::ModelUnavailable)
    );
}
#[test]
fn cooling_route_outside_required_residency_is_not_a_retry_hint() {
    let p = RoutingPolicy {
        required_residency: Some("us".into()),
        ..Default::default()
    };
    let mut eu = candidate(1, 0, 1, "eu");
    eu.circuit_open = true;
    eu.cooldown_remaining_seconds = 5;
    assert_eq!(
        order_candidates(&p, vec![eu], [0; 32]),
        Err(InferenceError::ModelUnavailable)
    );
}
#[test]
fn retries_require_exact_explicit_residency_of_first_selected_route() {
    let p = RoutingPolicy {
        max_attempts: 3,
        ..Default::default()
    };
    assert_eq!(
        order_candidates(
            &p,
            vec![
                candidate(1, 0, 1, "us"),
                candidate(2, 1, 1, "unspecified"),
                candidate(3, 2, 1, "eu"),
                candidate(4, 3, 1, "us")
            ],
            [0; 32]
        )
        .unwrap()
        .deployment_ids,
        vec![Uuid::from_u128(1), Uuid::from_u128(4)]
    );
    assert_eq!(
        order_candidates(
            &p,
            vec![
                candidate(1, 0, 1, "unspecified"),
                candidate(2, 0, 1, "unspecified"),
                candidate(3, 0, 1, "us")
            ],
            [0; 32]
        )
        .unwrap()
        .deployment_ids,
        vec![Uuid::from_u128(1)]
    );
}
#[test]
fn failover_is_explicit_and_transport_ambiguity_is_separate_opt_in() {
    let mut p = RoutePlan {
        max_attempts: 3,
        ..Default::default()
    };
    assert!(may_failover(&p, InferenceError::Busy));
    assert!(!may_failover(&p, InferenceError::UpstreamUnavailable));
    p.allow_ambiguous_failover = true;
    assert!(may_failover(&p, InferenceError::UpstreamUnavailable));
    for e in [
        InferenceError::InvalidRequest,
        InferenceError::ModelUnavailable,
        InferenceError::Unsupported,
        InferenceError::Configuration,
        InferenceError::Timeout,
        InferenceError::UpstreamRejected,
        InferenceError::InvalidUpstream,
        InferenceError::Storage,
    ] {
        assert!(!may_failover(&p, e));
        assert_eq!(affects_health(Some(e)), e == InferenceError::Timeout);
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
    for p in [
        RoutingPolicy {
            strategy: "random".into(),
            ..Default::default()
        },
        RoutingPolicy {
            max_attempts: 0,
            ..Default::default()
        },
        RoutingPolicy {
            max_attempts: 4,
            ..Default::default()
        },
        RoutingPolicy {
            failure_threshold: 0,
            ..Default::default()
        },
        RoutingPolicy {
            cooldown_seconds: 0,
            ..Default::default()
        },
        RoutingPolicy {
            cooldown_seconds: 3601,
            ..Default::default()
        },
    ] {
        assert_eq!(p.validate(), Err(InferenceError::Configuration));
    }
    for r in ["", "US", " us", "us ", "unspecified ", "us/east"] {
        assert!(!valid_residency(r));
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
    use crate::governance::tests::db::fixture;
    use crate::inference::repository::InferenceRepository;
    use sqlx::PgPool;
    #[sqlx::test(migrations = "./enterprise_migrations")]
    async fn circuit_opens_expires_and_success_resets(pool: PgPool) {
        let f = fixture(pool).await;
        let deployment = f
            .store
            .deployments(&f.principal, "company/smart")
            .await
            .unwrap()
            .remove(0);
        sqlx::query("INSERT INTO deployment_routing(deployment_id,failure_threshold,cooldown_seconds) VALUES($1,2,60)").bind(deployment.id).execute(&f.store.pool).await.unwrap();
        assert!(
            health(&f.store, deployment.id)
                .await
                .unwrap()
                .last_observed_at
                .is_none()
        );
        record_result(&f.store, deployment.id, Some(InferenceError::Busy))
            .await
            .unwrap();
        assert!(!health(&f.store, deployment.id).await.unwrap().circuit_open);
        record_result(&f.store, deployment.id, Some(InferenceError::Timeout))
            .await
            .unwrap();
        let h = health(&f.store, deployment.id).await.unwrap();
        assert!(h.circuit_open);
        assert_eq!(h.consecutive_failures, 2);
        // The model exists; its only route is cooling down (60 s policy).
        match plan(
            &f.store,
            &f.principal,
            "company/smart",
            std::slice::from_ref(&deployment),
            Uuid::new_v4(),
        )
        .await
        {
            Err(InferenceError::RouteCoolingDown(seconds)) => assert!((1..=60).contains(&seconds)),
            other => panic!("expected cooldown, got {other:?}"),
        }
        record_result(&f.store, deployment.id, Some(InferenceError::Storage))
            .await
            .unwrap();
        assert_eq!(
            health(&f.store, deployment.id)
                .await
                .unwrap()
                .consecutive_failures,
            2
        );
        sqlx::query("UPDATE deployment_health SET open_until=now()-interval '1 second'")
            .execute(&f.store.pool)
            .await
            .unwrap();
        assert!(
            plan(
                &f.store,
                &f.principal,
                "company/smart",
                std::slice::from_ref(&deployment),
                Uuid::new_v4()
            )
            .await
            .is_ok()
        );
        record_result(
            &f.store,
            deployment.id,
            Some(InferenceError::UpstreamUnavailable),
        )
        .await
        .unwrap();
        assert!(health(&f.store, deployment.id).await.unwrap().circuit_open);
        record_result(&f.store, deployment.id, None).await.unwrap();
        let h = health(&f.store, deployment.id).await.unwrap();
        assert_eq!(h.consecutive_failures, 0);
        assert!(h.open_until.is_none());
        assert!(h.last_observed_at.is_some());
        sqlx::query("UPDATE deployments SET enabled=false")
            .execute(&f.store.pool)
            .await
            .unwrap();
        assert_eq!(
            plan(
                &f.store,
                &f.principal,
                "company/smart",
                &[deployment],
                Uuid::new_v4()
            )
            .await,
            Err(InferenceError::ModelUnavailable)
        );
    }
    #[sqlx::test(migrations = "./enterprise_migrations")]
    async fn concurrent_failures_are_atomic_and_health_is_global(pool: PgPool) {
        let f = fixture(pool).await;
        let deployment = f
            .store
            .deployments(&f.principal, "company/smart")
            .await
            .unwrap()
            .remove(0);
        let results = futures_util::future::join_all(
            (0..24).map(|_| record_result(&f.store, deployment.id, Some(InferenceError::Busy))),
        )
        .await;
        assert!(results.iter().all(Result::is_ok));
        assert_eq!(
            health(&f.store, deployment.id)
                .await
                .unwrap()
                .consecutive_failures,
            24
        );
        record_result(&f.store, deployment.id, None).await.unwrap();
        assert_eq!(
            health(&f.store, deployment.id)
                .await
                .unwrap()
                .consecutive_failures,
            0
        );
        sqlx::query("INSERT INTO routing_policies(model_id,max_attempts) VALUES($1,2)")
            .bind(f.model)
            .execute(&f.store.pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM workspace_model_grants WHERE workspace_id=$1")
            .bind(f.team.workspace_id)
            .execute(&f.store.pool)
            .await
            .unwrap();
        assert_eq!(
            plan(
                &f.store,
                &f.team,
                "company/smart",
                std::slice::from_ref(&deployment),
                Uuid::new_v4()
            )
            .await,
            Err(InferenceError::ModelUnavailable)
        );
        sqlx::query("INSERT INTO workspace_model_grants(workspace_id,model_id,source) VALUES($1,$2,'direct')").bind(f.team.workspace_id).bind(f.model).execute(&f.store.pool).await.unwrap();
        assert_eq!(
            plan(
                &f.store,
                &f.team,
                "company/smart",
                &[deployment],
                Uuid::new_v4()
            )
            .await
            .unwrap()
            .max_attempts,
            2
        );
    }
}
