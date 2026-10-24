use open_model_gateway::config::{AlertWebhookPolicy, Environment};

#[test]
fn webhook_privilege_probes_take_management_lock_before_touching_saved_destinations() {
    let sql = open_model_gateway::store::VERIFY_PRIVILEGES_SQL;
    let lock = sql.find("SELECT lock_installation();").unwrap();
    let write = sql.find("UPDATE alert_webhook_destinations SET").unwrap();
    assert!(
        lock < write,
        "rollback-only probes must retain management lock order too"
    );
}

#[test]
fn webhook_url_limit_applies_after_canonical_percent_encoding() {
    let url = format!("https://example.test/{}", "é".repeat(500));
    assert!(url.len() < 2048);
    assert!(AlertWebhookPolicy::parse(&[url], vec![], Environment::Production).is_err());
}
