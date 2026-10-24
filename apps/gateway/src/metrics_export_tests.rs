use super::*;
use serde_json::{Value, json};
use std::collections::BTreeSet;

fn encoder() -> Encoder {
    Encoder::new(crate::config::Environment::Development, 123).unwrap()
}
fn encode(text: &str, traffic: bool) -> (Value, Drops) {
    let encoded = encoder().encode(Snapshot::parse(text, traffic), 456);
    (
        serde_json::from_slice(&encoded.body).unwrap(),
        encoded.dropped,
    )
}
fn metrics(body: &Value) -> &[Value] {
    body["resourceMetrics"][0]["scopeMetrics"][0]["metrics"]
        .as_array()
        .unwrap()
}
fn points(metric: &Value) -> &[Value] {
    ["sum", "gauge", "histogram"]
        .into_iter()
        .find_map(|kind| metric[kind]["dataPoints"].as_array())
        .unwrap()
}

#[test]
fn registration_inventory_includes_families_without_instantiated_series() {
    use syn::visit::Visit;
    #[derive(Default)]
    struct Registrations(BTreeSet<String>);
    impl<'ast> Visit<'ast> for Registrations {
        fn visit_expr_method_call(&mut self, call: &'ast syn::ExprMethodCall) {
            if call.method == "register" {
                if let Some(syn::Expr::Lit(syn::ExprLit {
                    lit: syn::Lit::Str(name),
                    ..
                })) = call.args.first()
                {
                    assert!(self.0.insert(format!("gateway_{}", name.value())));
                } else {
                    panic!("metric registrations must have literal names");
                }
            }
            syn::visit::visit_expr_method_call(self, call);
        }
    }
    let mut registrations = Registrations::default();
    registrations.visit_file(&syn::parse_file(include_str!("metrics.rs")).unwrap());
    assert_eq!(
        registrations.0,
        CLASSES.iter().map(|c| c.name.to_owned()).collect()
    );
}

#[test]
fn every_registered_family_classified_once_with_pinned_class_type_and_keys() {
    let golden = include_str!("metrics_families.golden");
    assert_eq!(CLASSES.len(), golden.lines().count());
    let mut names = BTreeSet::new();
    for class in CLASSES {
        assert!(names.insert(class.name));
        let keys = if class.labels.is_empty() {
            "-".into()
        } else {
            class.labels.join(",")
        };
        assert!(
            golden.lines().any(|line| line
                == format!(
                    "{} {} {} {:?}",
                    class.name,
                    class.kind.as_str(),
                    keys,
                    class.class
                )),
            "{}",
            class.name
        );
    }
}

#[test]
fn denylist_covers_all_classes_including_excluded_with_exact_path_exception() {
    assert_eq!(
        PATH_FAMILIES,
        &[
            "gateway_scope_lock_waiting",
            "gateway_scope_lock_wait_seconds",
            "gateway_lock_deadlock_retries"
        ]
    );
    for class in CLASSES {
        for key in class.labels {
            let denied = ["workspace", "user", "key", "api_key", "session"]
                .iter()
                .any(|prefix| key.starts_with(prefix))
                || [
                    "request_id",
                    "trace_id",
                    "execution_id",
                    "email",
                    "host",
                    "url",
                    "path",
                    "ip",
                ]
                .contains(key);
            assert!(
                !denied || (*key == "path" && PATH_FAMILIES.contains(&class.name)),
                "{} {key}",
                class.name
            );
        }
        for key in [
            "workspace_id",
            "user_id",
            "key_id",
            "api_key",
            "request_id",
            "trace_id",
            "execution_id",
            "email",
            "session_id",
            "host",
            "url",
            "ip",
            "path",
        ] {
            let labels = vec![(key.into(), "admission".into())];
            assert_eq!(
                labels_allowed(class, &labels),
                key == "path" && PATH_FAMILIES.contains(&class.name)
            );
        }
    }
}

#[test]
fn exact_wire_shape_and_large_integer_preserved() {
    let (body, dropped) = encode(
        "# TYPE gateway_config_changes counter\ngateway_config_changes_total{topic=\"models\"} 9007199254740993\n",
        false,
    );
    assert_eq!(dropped.total(), 0);
    let resource = &body["resourceMetrics"][0];
    let attrs = resource["resource"]["attributes"].as_array().unwrap();
    assert_eq!(attrs.len(), 4);
    assert_eq!(
        &attrs[..3],
        &[
            json!({"key":"service.name","value":{"stringValue":"open-model-gateway"}}),
            json!({"key":"service.version","value":{"stringValue":env!("CARGO_PKG_VERSION")}}),
            json!({"key":"deployment.environment.name","value":{"stringValue":"development"}}),
        ]
    );
    assert_eq!(attrs[3]["key"], "service.instance.id");
    let id = uuid::Uuid::parse_str(attrs[3]["value"]["stringValue"].as_str().unwrap()).unwrap();
    assert_eq!(id.get_version_num(), 4);
    assert_eq!(id.get_variant(), uuid::Variant::RFC4122);
    assert_eq!(
        resource["scopeMetrics"][0]["scope"],
        json!({"name":"omg.metrics","version":env!("CARGO_PKG_VERSION")})
    );
    assert_eq!(
        metrics(&body),
        &[json!({"name":"gateway_config_changes","unit":"1","sum":{
            "aggregationTemporality":2,"isMonotonic":true,"dataPoints":[{
                "attributes":[{"key":"topic","value":{"stringValue":"models"}}],
                "startTimeUnixNano":"123","timeUnixNano":"456","asInt":"9007199254740993"
            }]
        }})]
    );
    let (again, _) = encode("", false);
    assert_eq!(
        again["resourceMetrics"][0]["resource"],
        resource["resource"]
    );
}

#[test]
fn histogram_arithmetic_and_gauge_shape() {
    let text = "gateway_scope_lock_wait_seconds_sum{path=\"admission\",lock=\"rows\"} 4.5\ngateway_scope_lock_wait_seconds_count{lock=\"rows\",path=\"admission\"} 7\ngateway_scope_lock_wait_seconds_bucket{lock=\"rows\",path=\"admission\",le=\"+Inf\"} 7\ngateway_scope_lock_wait_seconds_bucket{lock=\"rows\",path=\"admission\",le=\"2\"} 5\ngateway_scope_lock_wait_seconds_bucket{lock=\"rows\",path=\"admission\",le=\"1\"} 2\ngateway_db_pool_max_connections -2\n";
    let (body, dropped) = encode(text, true);
    assert_eq!(dropped.total(), 0);
    let histogram = metrics(&body)
        .iter()
        .find(|m| m["histogram"].is_object())
        .unwrap();
    assert_eq!(histogram["unit"], "s");
    assert_eq!(histogram["histogram"]["aggregationTemporality"], 2);
    let point = &points(histogram)[0];
    assert_eq!(point["explicitBounds"], json!([1.0, 2.0]));
    assert_eq!(point["bucketCounts"], json!(["2", "3", "2"]));
    assert_eq!(point["sum"], 4.5);
    assert_eq!(point["count"], "7");
    assert_eq!(
        point["explicitBounds"].as_array().unwrap().len() + 1,
        point["bucketCounts"].as_array().unwrap().len()
    );
    let count: u64 = point["bucketCounts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().parse::<u64>().unwrap())
        .sum();
    assert_eq!(
        count,
        point["count"].as_str().unwrap().parse::<u64>().unwrap()
    );
    assert_eq!(points(&metrics(&body)[0])[0]["asInt"], "-2");
    assert!(
        metrics(&body)[0]["gauge"]
            .get("aggregationTemporality")
            .is_none()
    );
}

#[test]
fn traffic_default_and_build_info_are_absent() {
    let text = "gateway_http_requests_total{method=\"POST\",route=\"/v1/chat/completions\",status=\"200\"} 1\ngateway_build_info{version=\"x\"} 1\ngateway_config_listener_up 1\n";
    let (body, dropped) = encode(text, false);
    assert_eq!(metrics(&body).len(), 1);
    assert_eq!(metrics(&body)[0]["name"], "gateway_config_listener_up");
    assert_eq!(dropped.total(), 0);
    assert_eq!(metrics(&encode(text, true).0).len(), 2);
}

#[test]
fn disabled_families_skip_even_malformed_labels_without_drops() {
    for sample in [
        "{email=\"private@example.invalid\"} 1",
        "{method=\"unterminated} 1",
    ] {
        let (body, drops) = encode(
            &format!("gateway_http_requests_total{sample}\ngateway_build_info{sample}\n"),
            false,
        );
        assert!(metrics(&body).is_empty());
        assert_eq!(drops.total(), 0);
    }
}

#[test]
fn labels_escape_safely_and_identity_canaries_never_escape() {
    let canary = "07980fdd-1f4a-4716-bff3-20317aa86598";
    for class in CLASSES {
        let name = match class.kind {
            Kind::Counter => format!("{}_total", class.name),
            Kind::Histogram => format!("{}_count", class.name),
            Kind::Gauge => class.name.into(),
        };
        for key in [
            "workspace_id",
            "user_id",
            "api_key",
            "email",
            "request_id",
            "trace_id",
            "host",
            "url",
            "path",
        ] {
            let (body, drops) = encode(&format!("{name}{{{key}=\"{canary}\"}} 1\n"), true);
            assert!(metrics(&body).is_empty());
            assert!(!body.to_string().contains(canary));
            assert_eq!(
                drops.get(DropReason::Label),
                u64::from(class.class != Class::Excluded),
                "{} {key}",
                class.name
            );
        }
    }
    let (body, drops) = encode(
        "gateway_config_changes_total{topic=\"a\\\"b\\\\c\"} 1\n",
        false,
    );
    assert_eq!(drops.total(), 0);
    assert_eq!(
        points(&metrics(&body)[0])[0]["attributes"][0]["value"]["stringValue"],
        "a\"b\\c"
    );
    for value in [
        "/actual/path".into(),
        "x".repeat(97),
        "a\\nb".into(),
        "a\rb".into(),
    ] {
        let (_, dropped) = encode(
            &format!("gateway_lock_deadlock_retries_total{{path=\"{value}\"}} 1\n"),
            true,
        );
        assert_eq!(dropped.get(DropReason::Label), 1);
    }
    for value in ["admission", "settlement", "reconciliation"] {
        assert_eq!(
            metrics(
                &encode(
                    &format!("gateway_lock_deadlock_retries_total{{path=\"{value}\"}} 1\n"),
                    false
                )
                .0
            )
            .len(),
            1
        );
    }
}

#[test]
fn invalid_histogram_labels_count_one_logical_series_not_each_component() {
    let (body, drops) = encode(
        "gateway_scope_lock_wait_seconds_bucket{path=\"/private\",le=\"1\"} 1\ngateway_scope_lock_wait_seconds_bucket{path=\"/private\",le=\"+Inf\"} 1\ngateway_scope_lock_wait_seconds_count{path=\"/private\"} 1\ngateway_scope_lock_wait_seconds_sum{path=\"/private\"} 1\n",
        true,
    );
    assert!(metrics(&body).is_empty());
    assert_eq!(drops.get(DropReason::Label), 1);
}

#[test]
fn ordinary_label_length_is_in_characters_and_controls_are_refused() {
    for value in ["x".repeat(96), "é".repeat(96)] {
        let (body, drops) = encode(
            &format!("gateway_config_changes_total{{topic=\"{value}\"}} 1\n"),
            false,
        );
        assert_eq!(drops.total(), 0);
        assert_eq!(metrics(&body).len(), 1);
    }
    for value in [
        "x".repeat(97),
        "é".repeat(97),
        "x\\ny".into(),
        "x\ty".into(),
        "x\u{7f}y".into(),
    ] {
        let (body, drops) = encode(
            &format!("gateway_config_changes_total{{topic=\"{value}\"}} 1\n"),
            false,
        );
        assert!(metrics(&body).is_empty());
        assert_eq!(drops.get(DropReason::Label), 1);
    }
    for class in CLASSES {
        if class.labels.contains(&"path") {
            for value in ["admission", "settlement", "reconciliation"] {
                assert!(labels_allowed(class, &vec![("path".into(), value.into())]));
            }
            assert!(!labels_allowed(
                class,
                &vec![("path".into(), "/secret".into())]
            ));
        }
    }
}

#[test]
fn histogram_unsigned_counts_remain_exact_above_f64_precision() {
    let (body, drops) = encode(
        "gateway_scope_lock_wait_seconds_bucket{le=\"1\"} 9007199254740993\ngateway_scope_lock_wait_seconds_bucket{le=\"+Inf\"} 18446744073709551615\ngateway_scope_lock_wait_seconds_count 18446744073709551615\ngateway_scope_lock_wait_seconds_sum 1\n",
        true,
    );
    assert_eq!(drops.total(), 0);
    let point = &points(&metrics(&body)[0])[0];
    assert_eq!(point["count"], "18446744073709551615");
    assert_eq!(
        point["bucketCounts"],
        json!(["9007199254740993", "18437736874454810622"])
    );
}

#[test]
fn invalid_values_and_unclassified_are_counted() {
    for value in ["NaN", "+Inf", "-Inf", "-1", "1.2", "18446744073709551615"] {
        let (body, drops) = encode(
            &format!("gateway_config_poll_failures_total {value}\n"),
            false,
        );
        assert!(metrics(&body).is_empty());
        assert_eq!(drops.get(DropReason::InvalidValue), 1);
    }
    let (_, drops) = encode("gateway_future_total 1\n", false);
    assert_eq!(drops.get(DropReason::Unclassified), 1);
}

#[test]
fn caps_are_sorted_and_deterministic() {
    let lines = (0..305)
        .map(|i| format!("gateway_config_changes_total{{topic=\"{i:03}\"}} {i}\n"))
        .collect::<Vec<_>>();
    let (first, drops) = encode(&lines.concat(), false);
    let (second, again) = encode(&lines.into_iter().rev().collect::<String>(), false);
    assert_eq!(first, second);
    assert_eq!(drops, again);
    assert_eq!(drops.get(DropReason::SeriesLimit), 5);
    assert_eq!(points(&metrics(&first)[0]).len(), 300);
    let text = CLASSES
        .iter()
        .filter(|c| {
            !c.labels.is_empty()
                && c.class != Class::Excluded
                && c.kind == Kind::Counter
                && !c.labels.contains(&"path")
        })
        .take(8)
        .flat_map(|c| {
            (0..300).map(move |i| format!("{}_total{{{}=\"{i:03}\"}} 1\n", c.name, c.labels[0]))
        })
        .collect::<String>();
    let (body, drops) = encode(&text, true);
    assert_eq!(
        metrics(&body)
            .iter()
            .map(|m| points(m).len())
            .sum::<usize>(),
        2000
    );
    assert_eq!(drops.get(DropReason::SeriesLimit), 400);
    let snapshot = Snapshot::parse(&text, true);
    let limited = encoder().encode_with_limit(snapshot, 456, 2048);
    assert!(limited.body.len() <= 2048);
    assert!(limited.dropped.get(DropReason::BodyLimit) > 0);
    let standard = encoder().encode(Snapshot::parse(&text, true), 456);
    assert!(standard.body.len() <= 1024 * 1024);
}

#[test]
fn system_families_precede_traffic_under_point_and_body_caps() {
    let mut lines = [
        "gateway_admission_denials",
        "gateway_alert_email_attempts",
        "gateway_auth_throttled",
        "gateway_batch_lines",
        "gateway_batch_route_pauses",
        "gateway_batches",
        "gateway_cache_lookups",
        "gateway_http_requests",
    ]
    .into_iter()
    .flat_map(|name| {
        let class = CLASSES.iter().find(|c| c.name == name).unwrap();
        assert_eq!(class.class, Class::Traffic);
        (0..300).map(move |i| format!("{name}_total{{{}=\"{i:03}\"}} 1\n", class.labels[0]))
    })
    .collect::<Vec<_>>();
    lines.push("gateway_work_lease_terms_total{lease=\"x\"} 1\n".into());
    let text = lines.concat();
    let (body, drops) = encode(&text, true);
    assert!(
        metrics(&body)
            .iter()
            .any(|m| m["name"] == "gateway_work_lease_terms"),
        "Traffic must not starve the lease point"
    );
    assert_eq!(metrics(&body)[0]["name"], "gateway_work_lease_terms");
    assert_eq!(points(&metrics(&body)[0])[0]["asInt"], "1");
    assert_eq!(
        metrics(&body)
            .iter()
            .map(|m| points(m).len())
            .sum::<usize>(),
        MAX_POINTS
    );
    assert_eq!(drops.get(DropReason::SeriesLimit), 401);
    let reversed = lines.into_iter().rev().collect::<String>();
    assert_eq!((body, drops), encode(&reversed, true));
    let limited = encoder().encode_with_limit(Snapshot::parse(&text, true), 456, 2048);
    let body: Value = serde_json::from_slice(&limited.body).unwrap();
    assert_eq!(metrics(&body)[0]["name"], "gateway_work_lease_terms");
    assert!(limited.dropped.get(DropReason::BodyLimit) > 0);
}

#[test]
fn malformed_histograms_are_not_fabricated() {
    for (buckets, count) in [
        ("", 1),
        ("le=\"1\"} 2\n", 1),
        ("le=\"+Inf\"} 2\n", 1),
        // +Inf matches count: only the decreasing cumulative buckets are invalid.
        (
            "le=\"1\"} 3\ngateway_scope_lock_wait_seconds_bucket{le=\"+Inf\"} 2\n",
            2,
        ),
    ] {
        let prefix = if buckets.is_empty() {
            "".into()
        } else {
            format!("gateway_scope_lock_wait_seconds_bucket{{{buckets}")
        };
        let (body, drops) = encode(
            &format!(
                "{prefix}gateway_scope_lock_wait_seconds_count {count}\ngateway_scope_lock_wait_seconds_sum 1\n"
            ),
            true,
        );
        assert!(metrics(&body).is_empty());
        assert_eq!(drops.get(DropReason::InvalidValue), 1);
    }
}

#[tokio::test]
async fn actual_prometheus_render_round_trips_without_value_loss() {
    let m = crate::metrics::characterization_tests::populated_registry();
    m.observe_config_change("otlp-roundtrip");
    m.observe_deadlock_retry("settlement");
    drop(m.scope_lock_wait("rows", "reconciliation"));
    m.observe_http(
        &axum::http::Method::GET,
        "/health/live",
        200,
        std::time::Duration::from_millis(5),
    );
    let text = m.render(None).await;
    for traffic in [false, true] {
        let encoded = encoder().encode(Snapshot::parse(&text, traffic), 456);
        assert_eq!(encoded.dropped.total(), 0, "{text}");
        let body: Value = serde_json::from_slice(&encoded.body).unwrap();
        assert_eq!(
            metrics(&body)
                .iter()
                .map(|metric| metric["name"].as_str().unwrap())
                .collect::<BTreeSet<_>>(),
            CLASSES
                .iter()
                .filter(|class| class.class == Class::System
                    || (traffic && class.class == Class::Traffic))
                .map(|class| class.name)
                .collect(),
            "the populated render must export exactly the selected families"
        );
        for metric in metrics(&body) {
            let name = metric["name"].as_str().unwrap();
            let class = CLASSES.iter().find(|c| c.name == name).unwrap();
            assert!(traffic || class.class == Class::System);
            assert_eq!(metric["unit"], class.unit);
            for point in points(metric) {
                let mut labels = point["attributes"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|a| {
                        (
                            a["key"].as_str().unwrap(),
                            a["value"]["stringValue"].as_str().unwrap(),
                        )
                    })
                    .collect::<Vec<_>>();
                labels.sort();
                assert_eq!(
                    labels.iter().map(|(key, _)| *key).collect::<Vec<_>>(),
                    class.labels,
                    "{name}: exported label keys must match the golden inventory"
                );
                let sample_name = if class.kind == Kind::Counter {
                    format!("{name}_total")
                } else if class.kind == Kind::Histogram {
                    format!("{name}_count")
                } else {
                    name.into()
                };
                let expected = if class.kind == Kind::Histogram {
                    point["count"].as_str().unwrap().parse::<f64>().unwrap()
                } else if let Some(n) = point.get("asInt") {
                    n.as_str().unwrap().parse::<f64>().unwrap()
                } else {
                    point["asDouble"].as_f64().unwrap()
                };
                assert!(
                    text.lines().any(|line| {
                        if !line.starts_with(&sample_name) {
                            return false;
                        }
                        let Ok((source_labels, value)) = sample(&line[sample_name.len()..]) else {
                            return false;
                        };
                        source_labels
                            .iter()
                            .map(|(k, v)| (k.as_str(), v.as_str()))
                            .collect::<Vec<_>>()
                            == labels
                            && value.parse::<f64>().ok() == Some(expected)
                    }),
                    "{name}: {point}"
                );
            }
        }
    }
}

// DB refresh instantiates gauges absent from a render without a store. Use a
// private registry so unrelated global lease tests cannot clear them. The writer
// records into both registries; only its private writes exercise contention with
// this refresh, parsing and encoding. Validate both privacy selections.
#[cfg(feature = "integration-tests")]
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn db_refresh_round_trips_with_concurrent_global_recording(pool: sqlx::PgPool) {
    let m = crate::metrics::characterization_tests::fresh_registry();
    let (started, ready) = tokio::sync::oneshot::channel();
    let (stop, stopped) = std::sync::mpsc::channel();
    let writer = tokio::task::spawn_blocking(move || {
        crate::metrics::METRICS.observe_config_change("otlp-concurrent");
        started.send(()).unwrap();
        while matches!(
            stopped.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ) {
            crate::metrics::METRICS.observe_config_change("otlp-concurrent");
            m.observe_config_change("otlp-concurrent");
            m.observe_http(
                &axum::http::Method::GET,
                "/health/live",
                200,
                std::time::Duration::from_millis(5),
            );
            std::thread::yield_now();
        }
    });
    ready.await.unwrap();
    let text = m.render(Some(&crate::store::Store::new(pool))).await;
    // Compare one render against its own parse, never a later counter read.
    let encoded = [false, true].map(|traffic| {
        (
            traffic,
            encoder().encode(Snapshot::parse(&text, traffic), 456),
        )
    });
    stop.send(()).unwrap();
    writer.await.unwrap();
    for name in [
        "gateway_alert_delivery_pending_buckets",
        "gateway_alert_delivery_oldest_pending_age_seconds",
    ] {
        assert!(text.contains(&format!("{name}{{}} ")));
    }
    for (traffic, encoded) in encoded {
        assert_eq!(encoded.dropped.total(), 0, "{:?}", encoded.dropped);
        let body: Value = serde_json::from_slice(&encoded.body).unwrap();
        assert!(metrics(&body).iter().all(|metric| {
            CLASSES.iter().any(|class| {
                metric["name"] == class.name
                    && (class.class == Class::System || (traffic && class.class == Class::Traffic))
            })
        }));
        for name in [
            "gateway_reservations_held",
            "gateway_alert_email_deliveries",
            "gateway_alert_delivery_pending_buckets",
            "gateway_alert_delivery_oldest_pending_age_seconds",
        ] {
            let metric = metrics(&body).iter().find(|metric| metric["name"] == name);
            if !traffic {
                assert!(metric.is_none(), "{name} must be opt-in");
                continue;
            }
            let metric = metric.unwrap_or_else(|| panic!("DB gauge missing: {name}"));
            for point in points(metric) {
                let labels = point["attributes"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|a| {
                        (
                            a["key"].as_str().unwrap().to_owned(),
                            a["value"]["stringValue"].as_str().unwrap().to_owned(),
                        )
                    })
                    .collect::<Labels>();
                let expected = point["asInt"].as_str().unwrap();
                assert!(
                    text.lines().any(|line| {
                        line.strip_prefix(name).is_some_and(|rest| {
                            sample(rest).is_ok_and(|(source_labels, value)| {
                                source_labels == labels && value == expected
                            })
                        })
                    }),
                    "{name}: {point}"
                );
            }
        }
    }
}

#[test]
fn real_body_limit_keeps_a_sorted_prefix_of_histogram_points() {
    let families = CLASSES.iter().filter(|c| c.kind == Kind::Histogram);
    let mut lines = Vec::new();
    for class in families {
        let key = class.labels.iter().find(|key| **key != "path").unwrap();
        for i in 0..300 {
            let label = format!("{key}=\"{i:03}{}\"", "x".repeat(90));
            for bound in 0..100 {
                lines.push(format!(
                    "{}_bucket{{{label},le=\"{bound}\"}} {bound}\n",
                    class.name
                ));
            }
            lines.push(format!("{}_bucket{{{label},le=\"+Inf\"}} 100\n{}_count{{{label}}} 100\n{}_sum{{{label}}} 100\n", class.name, class.name, class.name));
        }
    }
    let first = encoder().encode(Snapshot::parse(&lines.concat(), true), 456);
    let second = encoder().encode(
        Snapshot::parse(&lines.into_iter().rev().collect::<String>(), true),
        456,
    );
    assert_eq!(first.body, second.body);
    assert_eq!(first.dropped, second.dropped);
    assert!(first.body.len() <= MAX_BODY);
    assert!(first.dropped.get(DropReason::BodyLimit) > 0);
    assert_eq!(first.dropped.get(DropReason::InvalidValue), 0);
    assert_eq!(first.points as u64 + first.dropped.total(), 1800);
}

// Like holds_tests::queue_cursor_replica_worker, re-exec exactly one helper:
// INSTANCE is process-global, so no test ordering/reset of its LazyLock is safe.
#[tokio::test]
async fn runtime_entropy_failure_is_quiet_and_cached_in_a_fresh_process() {
    let output = tokio::time::timeout(
        Duration::from_secs(10),
        tokio::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "metrics_export::tests::runtime_entropy_failure_child",
                "--ignored",
                "--nocapture",
            ])
            .kill_on_drop(true)
            .output(),
    )
    .await
    .expect("bounded entropy subprocess")
    .unwrap();
    assert!(
        output.status.success(),
        "entropy subprocess failed: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        output.stderr.is_empty(),
        "optional exporter entropy failure must not log"
    );
}

#[tokio::test]
#[ignore = "invoked only by runtime_entropy_failure_is_quiet_and_cached_in_a_fresh_process"]
async fn runtime_entropy_failure_child() {
    #[derive(Clone)]
    struct Logs(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
    impl std::io::Write for Logs {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let logs = Logs(Default::default());
    let writer = logs.clone();
    let store = unit_store().await;
    let config = crate::config::MetricsExportConfig {
        endpoint: Some((
            "http://127.0.0.1:1/v1/metrics".parse().unwrap(),
            reqwest::Client::new(),
        )),
        headers: Default::default(),
        interval: Duration::from_secs(10),
        traffic: false,
    };
    tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_max_level(tracing::Level::TRACE)
        .with_writer(move || writer.clone())
        .init();
    let baseline = tokio::runtime::Handle::current()
        .metrics()
        .num_alive_tasks();
    let fail = crate::entropy::seam::fail_on_this_thread();
    assert!(Runtime::start(config.clone(), Environment::Development, 1, store.clone()).is_none());
    assert_eq!(
        tokio::runtime::Handle::current()
            .metrics()
            .num_alive_tasks(),
        baseline,
        "no exporter task on entropy failure"
    );
    drop(fail);
    assert!(
        crate::entropy::bytes_quiet::<16>().is_ok(),
        "entropy seam has reset"
    );
    for _ in 0..3 {
        assert!(
            Runtime::start(config.clone(), Environment::Development, 1, store.clone()).is_none(),
            "cached failure must not draw a new successful identity"
        );
        assert_eq!(
            tokio::runtime::Handle::current()
                .metrics()
                .num_alive_tasks(),
            baseline
        );
    }
    assert!(
        logs.0.lock().unwrap().is_empty(),
        "no warning/error for optional identity failure"
    );
}

// Closed pool plus no metrics lease: render uses local pool gauges only, never
// a DB connection, DNS, or real PostgreSQL. Do not introduce a production hook.
async fn unit_store() -> crate::store::Store {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .connect_lazy("postgres://unused:unused@127.0.0.1:1/unused")
        .unwrap();
    pool.close().await;
    crate::store::Store::new(pool).with_leases(std::sync::Arc::new(crate::leases::Leases::new()))
}

#[tokio::test(start_paused = true)]
async fn slow_push_skips_overdue_ticks_without_catch_up_bursts() {
    // The configured minimum interval is 10s, but using 1s directly on the
    // worker makes its real 5s exchange exceed several ticks deterministically.
    // Poll/yield instead of awaiting socket readiness under paused time: Tokio
    // must not auto-advance a transport timeout while the OS handles loopback.
    async fn spin_until(mut condition: impl FnMut() -> bool) {
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while !condition() {
            assert!(
                std::time::Instant::now() < deadline,
                "loopback fixture did not progress"
            );
            tokio::task::yield_now().await;
        }
    }
    struct Abort(tokio::task::JoinHandle<()>);
    impl Drop for Abort {
        fn drop(&mut self) {
            self.0.abort();
        }
    }
    let store = unit_store().await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1/metrics", listener.local_addr().unwrap());
    let received = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let observed = received.clone();
    let closed = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let closed_by_server = closed.clone();
    let server = Abort(tokio::spawn(async move {
        let mut sockets = tokio::task::JoinSet::new();
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let observed = observed.clone();
            let closed = closed_by_server.clone();
            sockets.spawn(async move {
                use tokio::io::AsyncReadExt;
                let mut buf = [0; 4096];
                // Seeing request bytes (not just accept) proves the push is
                // in its HTTP exchange. Never answer; timeout closes it.
                assert!(socket.read(&mut buf).await.unwrap() > 0);
                observed.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                while socket.read(&mut buf).await.is_ok_and(|n| n > 0) {}
                closed.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            });
        }
    }));
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let exporter = Exporter {
        url: url.parse().unwrap(),
        client: reqwest::Client::builder().no_proxy().build().unwrap(),
        headers: Default::default(),
        encoder: encoder(),
        traffic: false,
    };
    let worker = Abort(tokio::spawn(super::worker(
        exporter,
        store,
        Duration::from_secs(1),
        stopped,
    )));
    tokio::task::yield_now().await; // install the first tick
    tokio::time::advance(Duration::from_secs(1)).await;
    spin_until(|| received.load(std::sync::atomic::Ordering::SeqCst) == 1).await;
    tokio::time::advance(EXCHANGE).await; // several ticks overdue; first push times out
    // EOF proves the worker consumed the timeout/cancelled its exchange.
    // It installs its next timer synchronously before yielding again.
    spin_until(|| closed.load(std::sync::atomic::Ordering::SeqCst) == 1).await;
    assert_eq!(received.load(std::sync::atomic::Ordering::SeqCst), 1);
    tokio::time::advance(Duration::from_millis(999)).await;
    for _ in 0..100 {
        tokio::task::yield_now().await;
    }
    assert_eq!(
        received.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "no immediate replay of overdue ticks"
    );
    tokio::time::advance(Duration::from_millis(1)).await;
    spin_until(|| received.load(std::sync::atomic::Ordering::SeqCst) == 2).await;
    // The second request stays in-flight even after the skipped-tick boundary;
    // there is only one worker, never overlapping/catch-up exports.
    for _ in 0..100 {
        tokio::task::yield_now().await;
    }
    assert_eq!(received.load(std::sync::atomic::Ordering::SeqCst), 2);
    drop(stop);
    worker.0.abort();
    server.0.abort();
}

proptest::proptest! {
    #[test]
    fn hostile_exposition_never_panics(text in "(?s).{0,4096}", label in "(?s).{0,120}", value in proptest::sample::select(vec!["NaN", "+Inf", "-Inf", "1", "9007199254740993"])) {
        let text = format!("{text}\ngateway_config_changes_total{{topic=\"{label}\"}} {value}\n");
        let encoded = encoder().encode(Snapshot::parse(&text, true), 456);
        proptest::prop_assert!(encoded.body.len() <= MAX_BODY);
        let _: Value = serde_json::from_slice(&encoded.body).unwrap();
    }
}
