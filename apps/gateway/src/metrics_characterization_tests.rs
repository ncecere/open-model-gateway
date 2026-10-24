//! #99's baseline, not an OTLP parser or an export classification table.
//! Coordinator amendment: `path` is retained on the two scope-lock families
//! and lock_deadlock_retries. Slice 2 must permit it ONLY there and ONLY for
//! admission/settlement/reconciliation; arbitrary HTTP paths remain forbidden.
use super::*;
use std::collections::{BTreeMap, BTreeSet};

/// The unit fixture primes DB gauges manually. Also prove their REAL refresh
/// path uses the golden keys, so a refresh-only label change cannot hide.
#[cfg(feature = "integration-tests")]
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn db_collector_label_keys_match_golden(pool: sqlx::PgPool) {
    let out = Metrics::new().render(Some(&Store::new(pool))).await;
    for family in [
        "gateway_db_pool_connections",
        "gateway_reservations_held",
        "gateway_alert_email_deliveries",
    ] {
        let golden = include_str!("metrics_families.golden")
            .lines()
            .find(|line| line.starts_with(&format!("{family} ")))
            .unwrap();
        let expected = golden.rsplit_once(' ').unwrap().1;
        let lines = out
            .lines()
            .filter(|line| line.starts_with(&format!("{family}{{")))
            .collect::<Vec<_>>();
        assert!(
            !lines.is_empty(),
            "DB collector must have refreshed {family}"
        );
        for line in lines {
            let labels = line.split_once('{').unwrap().1.split_once('}').unwrap().0;
            let keys = labels
                .split(',')
                .map(|label| label.split_once('=').unwrap().0)
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect::<Vec<_>>()
                .join(",");
            assert_eq!(keys, expected, "{family}");
        }
    }
}

#[tokio::test]
async fn prometheus_family_types_and_label_keys_match_golden() {
    // Isolated registry: parallel tests cannot add or remove fixture series.
    let m: &'static Metrics = Box::leak(Box::new(Metrics::new()));
    m.observe_http(
        &axum::http::Method::POST,
        "/v1/chat/completions",
        200,
        Duration::ZERO,
    );
    let labels = m.labels("openai", "public-model");
    m.observe_attempt(AttemptObservation {
        labels: &labels,
        outcome: "failed",
        error: Some(InferenceError::UpstreamUnavailable),
        input_tokens: Some(1),
        output_tokens: Some(1),
        generation_ms: Some(1),
        time_to_first_token_ms: Some(1),
    });
    m.observe_settlement(Settlement::Settled);
    m.observe_capacity_denial();
    m.observe_admission_phases(PhaseTimer::start(), "admitted");
    m.observe_settlement_phases(PhaseTimer::start(), "settled");
    m.observe_alert_run("ok", 0);
    m.observe_alert_delivery("delivered");
    m.observe_alert_webhook("sent");
    m.alert_webhook_queue
        .get_or_create(&[("state", "waiting".into())])
        .set(0);
    m.alert_webhook_oldest.get_or_create(&[]).set(0);
    m.observe_deadlock_retry("reconciliation");
    drop(m.scope_lock_wait("rows", "admission"));
    m.observe_collection_error("reservations");
    m.observe_file_store("local", "put", None);
    m.observe_file_store_bytes("local", "put", 1);
    m.observe_batch_created("gateway");
    m.observe_batch_lines("gateway", "openai", 1, 1);
    m.set_batch_queue("gateway", 0);
    m.set_batch_workers(1, 0);
    m.set_batch_route_lines(&[("openai".into(), 0, 0)]);
    m.set_batch_paused_routes(&[]);
    m.observe_batch_route_pause("workers");
    m.observe_cache("keys", "hit");
    m.observe_config_change("keys");
    m.auth_throttled("login", "pending_cap");
    m.key_issuance_refused("holder", "create");
    m.egress_denied("telemetry");
    m.audit_export_delivery("delivered");
    m.set_lease_held("metrics", true);
    m.observe_lease_term("metrics");
    m.observe_background_run("metrics", "ok");
    m.set_partition_months("requests", 2);
    m.observe_rollup_hours("new", 1);
    // These three families are populated only by the DB refresh; prime their
    // known state labels without requiring a DB for this characterization.
    m.pool_connections
        .get_or_create(&[("state", "idle".into())])
        .set(0);
    m.reservations
        .get_or_create(&[("state", "pending".into())])
        .set(0);
    m.alert_delivery_queue
        .get_or_create(&[("state", "retrying".into())])
        .set(0);

    // This small reader handles only our controlled fixture (no escaped label
    // values). Histograms contribute family labels, not the synthetic `le`.
    let text = m.render(None).await;
    let mut families = BTreeMap::<String, (String, BTreeSet<String>)>::new();
    let mut observed = BTreeSet::new();
    let mut current = None;
    for line in text.lines() {
        if let Some(line) = line.strip_prefix("# TYPE ") {
            let (name, kind) = line.split_once(' ').unwrap();
            assert!(
                families
                    .insert(name.into(), (kind.into(), BTreeSet::new()))
                    .is_none()
            );
            current = Some(name.to_owned());
        } else if !line.starts_with('#') && !line.is_empty() {
            let name = current.as_ref().unwrap();
            observed.insert(name.clone());
            if let Some((_, rest)) = line.split_once('{') {
                let (labels, _) = rest.split_once('}').unwrap();
                for label in labels.split(',') {
                    let (key, _) = label.split_once('=').unwrap();
                    if key != "le" {
                        families.get_mut(name).unwrap().1.insert(key.into());
                    }
                }
            }
        }
    }
    assert_eq!(
        observed,
        families.keys().cloned().collect(),
        "fixture must instantiate EVERY family"
    );
    let actual = families
        .into_iter()
        .map(|(name, (kind, keys))| {
            let keys = if keys.is_empty() {
                "-".into()
            } else {
                keys.into_iter().collect::<Vec<_>>().join(",")
            };
            format!("{name} {kind} {keys}\n")
        })
        .collect::<String>();
    assert_eq!(actual, include_str!("metrics_families.golden"));
}
