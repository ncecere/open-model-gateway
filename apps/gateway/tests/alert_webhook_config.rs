use open_model_gateway::config::{AlertWebhookPolicy, Environment};

#[test]
fn webhook_url_limit_applies_after_canonical_percent_encoding() {
    let url = format!("https://example.test/{}", "é".repeat(500));
    assert!(url.len() < 2048);
    assert!(AlertWebhookPolicy::parse(&[url], vec![], Environment::Production).is_err());
}
