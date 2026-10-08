#![cfg(feature = "integration-tests")]
use open_model_gateway::billing::BillingUsage;
use sqlx::PgPool;
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn billing_json_requires_all_nullable_counter_keys_in_storage(pool: PgPool) {
    let complete = serde_json::to_value(BillingUsage::default()).unwrap();
    assert!(
        sqlx::query_scalar::<_, bool>("SELECT valid_billing_usage($1)")
            .bind(&complete)
            .fetch_one(&pool)
            .await
            .unwrap()
    );
    assert!(
        !sqlx::query_scalar::<_, bool>("SELECT valid_billing_usage('{}'::jsonb)")
            .fetch_one(&pool)
            .await
            .unwrap()
    );
    for field in complete.as_object().unwrap().keys() {
        let mut missing = complete.clone();
        missing.as_object_mut().unwrap().remove(field);
        assert!(
            !sqlx::query_scalar::<_, bool>("SELECT valid_billing_usage($1)")
                .bind(missing)
                .fetch_one(&pool)
                .await
                .unwrap(),
            "{field}"
        );
    }
}
