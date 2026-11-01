use super::*;

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn incoherent_operator_usage_is_invalid_and_retains_hold(pool: PgPool) {
    let f = fixture(pool).await;
    f.price(1_000_000).await;
    let start = f.start();
    crate::governance::admit(&f.store, &start, &chat(), 30)
        .await
        .unwrap();
    crate::governance::finish(&f.store, &done(start.id, None, None))
        .await
        .unwrap();
    let path = format!(
        "/api/v1/workspaces/{}/costs/{}/reconcile",
        f.principal.workspace_id, start.id
    );
    let snapshot = || async {
        sqlx::query_scalar::<_, Value>(
            "SELECT to_jsonb(r) FROM governance_reservations r WHERE execution_id=$1",
        )
        .bind(start.id)
        .fetch_one(&f.store.pool)
        .await
        .unwrap()
    };
    let before = snapshot().await;
    assert!(
        before["state"] == "unknown",
        "fixture must retain unknown hold"
    );
    let max = "9223372036854775807";
    let mut failures = Vec::new();
    for (case, input, output, fields) in [
        (
            "uncached above total",
            "3",
            "2",
            json!({"total_input_tokens":"1","uncached_input_tokens":"5"}),
        ),
        (
            "write parts above aggregate",
            "3",
            "2",
            json!({"cache_write_input_tokens":"1","cache_write_default_input_tokens":"2"}),
        ),
        (
            "write parts do not sum",
            "3",
            "2",
            json!({"cache_write_input_tokens":"5","cache_write_default_input_tokens":"1","cache_write_5m_input_tokens":"1","cache_write_1h_input_tokens":"1"}),
        ),
        (
            "full breakdown does not sum",
            "3",
            "2",
            json!({"total_input_tokens":"10","uncached_input_tokens":"1","cache_read_input_tokens":"1","cache_write_input_tokens":"1"}),
        ),
        (
            "raw above total",
            "3",
            "2",
            json!({"total_input_tokens":"2"}),
        ),
        (
            "input breakdown overflow",
            "3",
            "2",
            json!({"uncached_input_tokens":max,"cache_read_input_tokens":"1"}),
        ),
        (
            "write parts overflow",
            "3",
            "2",
            json!({"cache_write_default_input_tokens":max,"cache_write_5m_input_tokens":"1"}),
        ),
        ("token sum overflow", max, "1", Value::Null),
    ] {
        let mut billing = serde_json::to_value(crate::billing::BillingUsage::default()).unwrap();
        if fields.is_null() {
            billing = Value::Null;
        } else {
            for (key, value) in fields.as_object().unwrap() {
                billing[key] = value.clone();
            }
        }
        let body = json!({"input_tokens":input,"output_tokens":output,"billing_usage":billing,"evidence":"receipt:test"});
        let (status, error) = call(&f, &user(f.owner, true), "POST", &path, body).await;
        if status != StatusCode::BAD_REQUEST {
            failures.push(format!("{case}: status {status}"));
        } else {
            assert!(
                error["error"]["message"] == "Invalid request",
                "{case}: invalid kind"
            );
        }
        assert!(snapshot().await == before, "{case}: hold unchanged");
    }
    let body = json!({"input_tokens":"3","output_tokens":"2","billing_usage":null,"evidence":"receipt:test"});
    let (status, _) = call(&f, &user(f.owner, true), "POST", &path, body).await;
    assert_eq!(status, StatusCode::OK, "coherent reconcile status");
    let after = snapshot().await;
    assert!(after["state"] == "settled", "coherent reconcile settles");
    assert!(after["actual_microusd"] == 5, "coherent reconcile cost");
    assert!(failures.is_empty(), "{}", failures.join("; "));
}
