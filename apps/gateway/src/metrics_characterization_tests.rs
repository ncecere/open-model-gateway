//! #99's Prometheus baseline; the golden's final column separately pins each
//! family's OTLP privacy class (not present in the Prometheus exposition).
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
        "gateway_alert_delivery_pending_buckets",
        "gateway_alert_delivery_oldest_pending_age_seconds",
    ] {
        let golden = include_str!("metrics_families.golden")
            .lines()
            .find(|line| line.starts_with(&format!("{family} ")))
            .unwrap();
        let expected = golden.split_whitespace().nth(2).unwrap();
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
                .filter(|label| !label.is_empty())
                .map(|label| label.split_once('=').unwrap().0)
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect::<Vec<_>>()
                .join(",");
            assert_eq!(
                if keys.is_empty() { "-" } else { &keys },
                expected,
                "{family}"
            );
        }
    }
}

#[tokio::test]
async fn otlp_self_metrics_are_precreated_fixed_enums_and_record_counts() {
    use crate::metrics_export::{Delivery, DropReason};
    let m = Metrics::new();
    let before = m.render(None).await;
    for outcome in Delivery::ALL {
        assert!(before.contains(&format!(
            "gateway_otlp_metrics_pushes_total{{outcome=\"{}\"}} 0",
            outcome.as_str()
        )));
        m.otlp_metrics_push(*outcome);
    }
    for reason in DropReason::ALL {
        assert!(before.contains(&format!(
            "gateway_otlp_metrics_dropped_total{{reason=\"{}\"}} 0",
            reason.as_str()
        )));
        m.otlp_metrics_drop(*reason, 3);
    }
    m.otlp_metrics_rejected(9007199254740993);
    let after = m.render(None).await;
    for outcome in Delivery::ALL {
        assert!(after.contains(&format!(
            "gateway_otlp_metrics_pushes_total{{outcome=\"{}\"}} 1",
            outcome.as_str()
        )));
    }
    for reason in DropReason::ALL {
        assert!(after.contains(&format!(
            "gateway_otlp_metrics_dropped_total{{reason=\"{}\"}} 3",
            reason.as_str()
        )));
    }
    assert!(after.contains("gateway_otlp_metrics_rejected_points_total 9007199254740993"));
}

pub(crate) fn fresh_registry() -> &'static Metrics {
    // ScopeLockWait requires a static registry; each caller gets its own.
    Box::leak(Box::new(Metrics::new()))
}

/// Private, fully populated registry shared by characterization and encoder
/// tests. Parallel gateway tests cannot introduce caps or privacy drops here.
pub(crate) fn populated_registry() -> &'static Metrics {
    let m = fresh_registry();
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
    // These five families are populated only by the DB refresh; prime their
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
    m.alert_pending_buckets.get_or_create(&[]).set(0);
    m.alert_oldest_pending_age.get_or_create(&[]).set(0);
    m
}

#[tokio::test]
async fn prometheus_family_types_and_label_keys_match_golden() {
    let m = populated_registry();
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
                for label in labels.split(',').filter(|label| !label.is_empty()) {
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
    let prometheus_golden = include_str!("metrics_families.golden")
        .lines()
        .map(|line| format!("{}\n", line.rsplit_once(' ').unwrap().0))
        .collect::<String>();
    assert_eq!(actual, prometheus_golden);
}
