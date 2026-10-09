use super::*;
use crate::governance::tests::db::{billing, done, fixture, rates, request};
use crate::inference::error::LimitScope;
use sqlx::PgPool;
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn missing_reservation_history_blocks_new_monetary_and_token_caps(pool: PgPool) {
    let f = fixture(pool).await;
    // The unreserved execution must fall in the admission minute.
    let now = f.store.freeze_admission_clock().await.unwrap();
    f.price(1_000_000).await;
    let old = f.start();
    sqlx::query("INSERT INTO inference_executions(id,workspace_id,api_key_id,deployment_id,public_model,provider,streamed,state,root_request_id,started_at) VALUES($1,$2,$3,$4,'company/smart','openai',false,'cancelled',$1,$5)").bind(old.id).bind(f.principal.workspace_id).bind(f.principal.key_id).bind(f.deployment).bind(now).execute(&f.store.pool).await.unwrap();
    for (tokens, budget) in [(None, Some(10000_i64)), (Some(10000_i64), None)] {
        sqlx::query("INSERT INTO workspace_local_policies(workspace_id,tokens_per_minute) VALUES($1,$2) ON CONFLICT(workspace_id) DO UPDATE SET tokens_per_minute=$2").bind(f.principal.workspace_id).bind(tokens).execute(&f.store.pool).await.unwrap();
        crate::governance::set_test_budget(
            &f.store.pool,
            "local",
            None,
            Some(f.principal.workspace_id),
            None,
            "month",
            budget,
        )
        .await;
        assert_eq!(
            admit(&f.store, &f.start(), &request(), 30).await,
            Err(if budget.is_some() {
                InferenceError::UnresolvedUsage(LimitScope::Workspace)
            } else {
                InferenceError::Busy
            })
        );
    }
}
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn every_counter_and_evidence_is_preserved_through_reconciliation(pool: PgPool) {
    let f = fixture(pool).await;
    f.v2(rates()).await;
    let start = f.start();
    admit(&f.store, &start, &request(), 30).await.unwrap();
    let mut partial = done(start.id, Some(10), Some(2));
    partial.usage.billing = Some(BillingUsage {
        cache_write_1h_input_tokens: None,
        ..billing()
    });
    finish(&f.store, &partial).await.unwrap();
    let good = Usage {
        input_tokens: Some(10),
        output_tokens: Some(2),
        billing: Some(billing()),
        ..Default::default()
    };
    let reduced = Usage {
        billing: Some(BillingUsage {
            cache_write_default_input_tokens: Some(2),
            cache_write_5m_input_tokens: Some(5),
            ..billing()
        }),
        ..good
    };
    assert_eq!(
        resolve_usage(
            &f.store,
            f.principal.workspace_id,
            start.id,
            reduced,
            "receipt:1",
            f.owner
        )
        .await,
        Err(InferenceError::InvalidRequest)
    );
    assert_eq!(
        resolve_usage(
            &f.store,
            f.principal.workspace_id,
            start.id,
            Usage {
                billing: None,
                ..good
            },
            "receipt:1",
            f.owner
        )
        .await,
        Err(InferenceError::InvalidRequest)
    );
    resolve_usage(
        &f.store,
        f.principal.workspace_id,
        start.id,
        good,
        "receipt:1",
        f.owner,
    )
    .await
    .unwrap();
    resolve_usage(
        &f.store,
        f.principal.workspace_id,
        start.id,
        good,
        "receipt:1",
        f.owner,
    )
    .await
    .unwrap();
    assert_eq!(
        resolve_usage(
            &f.store,
            f.principal.workspace_id,
            start.id,
            good,
            "receipt:2",
            f.owner
        )
        .await,
        Err(InferenceError::InvalidRequest)
    );
    let changed = Usage {
        billing: Some(BillingUsage {
            cache_write_default_input_tokens: Some(4),
            cache_write_5m_input_tokens: Some(3),
            ..billing()
        }),
        ..good
    };
    assert_eq!(
        resolve_usage(
            &f.store,
            f.principal.workspace_id,
            start.id,
            changed,
            "receipt:1",
            f.owner
        )
        .await,
        Err(InferenceError::InvalidRequest)
    );
}
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn every_attempt_pins_its_own_price_and_cost_center_snapshot(pool: PgPool) {
    let f = fixture(pool).await;
    let first = f.price(1_000_000).await;
    let cc = Uuid::new_v4();
    sqlx::query("INSERT INTO cost_centers(id,name,code) VALUES($1,'First','C1')")
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
    let a = f.start();
    admit(&f.store, &a, &request(), 30).await.unwrap();
    let mut failure = done(a.id, None, None);
    failure.outcome = Outcome::Failed;
    failure.error = Some(InferenceError::Busy);
    finish(&f.store, &failure).await.unwrap();
    let second = f.price(2_000_000).await;
    sqlx::query("UPDATE workspaces SET cost_center_id=NULL WHERE id=$1")
        .bind(f.principal.workspace_id)
        .execute(&f.store.pool)
        .await
        .unwrap();
    let mut b = f.start();
    b.root_request_id = a.root_request_id;
    b.attempt_number = 2;
    admit(&f.store, &b, &request(), 30).await.unwrap();
    finish(&f.store, &done(b.id, Some(3), Some(2)))
        .await
        .unwrap();
    let rows:Vec<(Uuid,Uuid,Option<Uuid>,Option<i64>)>=sqlx::query_as("SELECT e.id,r.price_id,e.cost_center_id,r.actual_microusd FROM inference_executions e JOIN governance_reservations r ON r.execution_id=e.id ORDER BY e.attempt_number").fetch_all(&f.store.pool).await.unwrap();
    assert_eq!(
        rows,
        vec![
            (a.id, first, Some(cc), None),
            (b.id, second, None, Some(10))
        ]
    );
}
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn unknown_cache_bound_denies_every_monetary_scope_before_any_execution(pool: PgPool) {
    let f = fixture(pool).await;
    f.v2(CachePricing {
        read: CacheRate::Unknown,
        ..rates()
    })
    .await;
    // Every budget layer (scope x period) requires a finite bound.
    sqlx::query("INSERT INTO workspace_platform_policy_overrides(workspace_id) VALUES($1)")
        .bind(f.principal.workspace_id)
        .execute(&f.store.pool)
        .await
        .unwrap();
    let ws = Some(f.principal.workspace_id);
    let key = Some(f.principal.key_id);
    for (layer, kind, w, k, period) in [
        ("installation", None, None, None, "lifetime"),
        ("override", None, ws, None, "day"),
        ("local", None, ws, None, "week"),
        ("key", None, ws, key, "month"),
    ] {
        crate::governance::set_test_budget(&f.store.pool, layer, kind, w, k, period, Some(10000))
            .await;
        assert_eq!(
            admit(&f.store, &f.start(), &request(), 30).await,
            Err(InferenceError::Configuration)
        );
        crate::governance::set_test_budget(&f.store.pool, layer, kind, w, k, period, None).await;
    }
    sqlx::query("DELETE FROM workspace_platform_policy_overrides")
        .execute(&f.store.pool)
        .await
        .unwrap();
    crate::governance::set_test_budget(
        &f.store.pool,
        "type",
        Some("personal"),
        None,
        None,
        "month",
        Some(10000),
    )
    .await;
    assert_eq!(
        admit(&f.store, &f.start(), &request(), 30).await,
        Err(InferenceError::Configuration)
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM inference_executions")
            .fetch_one(&f.store.pool)
            .await
            .unwrap(),
        0
    );
}
