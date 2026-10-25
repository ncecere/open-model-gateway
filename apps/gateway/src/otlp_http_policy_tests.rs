//! #313: exercise the policy actually selected by export_http_client, using
//! the production literal check and post-DNS filter, plus a local TCP collector.
use super::export_http_policy;
use crate::egress::{Networks, Policy, Purpose, check_url, filter_for_test};
use std::net::{IpAddr, SocketAddr};

const LOOPBACK: &[&str] = &["127.0.0.1", "127.1.2.3", "::1", "::ffff:127.0.0.1"];
const PUBLIC: &[&str] = &["8.8.8.8", "2606:4700::1111", "::ffff:8.8.8.8"];
const PRIVATE: &[&str] = &["10.1.2.3", "fd12::1", "100.64.0.1", "::ffff:10.1.2.3"];
const REFUSED: &[&str] = &[
    "0.0.0.0",
    "::",
    "169.254.169.254",
    "::ffff:169.254.169.254",
    "fd00:ec2::254",
    "fe80::1",
    "224.0.0.1",
    "ff02::1",
    "255.255.255.255",
    "240.0.0.1",
    "192.0.2.1",
    "2001:db8::1",
    "64:ff9b::a9fe:a9fe",
];

fn ip(address: &str) -> IpAddr {
    address.parse().unwrap()
}

fn sockets(addresses: &[&str]) -> Vec<SocketAddr> {
    addresses
        .iter()
        .map(|address| SocketAddr::new(ip(address), 4318))
        .collect()
}

fn general_policy() -> Policy {
    // Every private class is deliberately approved. HTTP must still narrow it.
    Policy::new(
        true,
        Networks::parse(
            "test private allowlist",
            "10.0.0.0/8,fd00::/8,100.64.0.0/10",
        )
        .unwrap(),
    )
}

fn url(scheme: &str, host: &str) -> reqwest::Url {
    let host = if host.contains(':') {
        format!("[{host}]")
    } else {
        host.to_owned()
    };
    reqwest::Url::parse(&format!("{scheme}://{host}:4318/v1/traces")).unwrap()
}

fn http_policy() -> Policy {
    export_http_policy(&url("http", "localhost"), general_policy())
}

#[test]
fn issue_313_http_accepts_loopback_dns_answers_and_literals() {
    let policy = http_policy();
    assert_eq!(
        filter_for_test(&policy, Purpose::Telemetry, "localhost", sockets(LOOPBACK)).unwrap(),
        sockets(LOOPBACK)
    );
    for &address in LOOPBACK {
        assert!(policy.allows(ip(address)), "{address}");
        let literal = url("http", address);
        let selected = export_http_policy(&literal, general_policy());
        assert!(
            check_url(&selected, Purpose::Telemetry, &literal).is_ok(),
            "{address}"
        );
    }
}

fn assert_http_dns_refused(addresses: &[&str]) {
    // #352: refuses Purpose::Telemetry destinations, which a `Capture`
    // session elsewhere asserts an exact count of; see
    // `log_capture_test`'s doc comment.
    let _lock = crate::log_capture_test::exclusive();
    let general = general_policy();
    for &address in addresses {
        assert!(
            general.allows(ip(address)),
            "general policy must allow {address}"
        );
        assert!(
            filter_for_test(
                &http_policy(),
                Purpose::Telemetry,
                "localhost",
                sockets(&[address])
            )
            .is_err(),
            "HTTP localhost must reject non-loopback DNS answer {address}"
        );
    }
}

#[test]
fn issue_313_http_rejects_public_dns_answers() {
    assert_http_dns_refused(PUBLIC);
}

#[test]
fn issue_313_http_rejects_allowlisted_private_dns_answers() {
    assert_http_dns_refused(PRIVATE);
}

fn assert_http_mixed_dns_refused(non_loopback: &[&str]) {
    // #352: refuses Purpose::Telemetry destinations; see
    // `log_capture_test`'s doc comment.
    let _lock = crate::log_capture_test::exclusive();
    for &address in non_loopback {
        // Test both orders: one permitted loopback must not mask a bad answer.
        for answers in [["127.0.0.1", address], [address, "::1"]] {
            assert!(
                filter_for_test(
                    &http_policy(),
                    Purpose::Telemetry,
                    "localhost",
                    sockets(&answers)
                )
                .is_err(),
                "HTTP localhost must reject the entire mixed DNS answer {answers:?}"
            );
        }
    }
}

#[test]
fn issue_313_http_rejects_mixed_loopback_and_public_dns_answers() {
    assert_http_mixed_dns_refused(PUBLIC);
}

#[test]
fn issue_313_http_rejects_mixed_loopback_and_allowlisted_private_dns_answers() {
    assert_http_mixed_dns_refused(PRIVATE);
}

#[test]
fn issue_313_http_rejects_mixed_loopback_and_reserved_dns_answers() {
    assert_http_mixed_dns_refused(REFUSED);
}

fn assert_http_literals_refused(addresses: &[&str]) {
    // #352: refuses Purpose::Telemetry destinations; see
    // `log_capture_test`'s doc comment.
    let _lock = crate::log_capture_test::exclusive();
    for &address in addresses {
        let literal = url("http", address);
        let general = general_policy();
        assert!(check_url(&general, Purpose::Telemetry, &literal).is_ok());
        let selected = export_http_policy(&literal, general);
        assert!(
            check_url(&selected, Purpose::Telemetry, &literal).is_err(),
            "HTTP must reject non-loopback literal {address}"
        );
    }
}

#[test]
fn issue_313_http_rejects_public_literals() {
    assert_http_literals_refused(PUBLIC);
}

#[test]
fn issue_313_http_rejects_allowlisted_private_literals() {
    assert_http_literals_refused(PRIVATE);
}

#[test]
fn issue_313_https_preserves_input_policy_decisions_for_every_address_class() {
    // #352: refuses Purpose::Telemetry destinations (the REFUSED class);
    // see `log_capture_test`'s doc comment.
    let _lock = crate::log_capture_test::exclusive();
    for general in [
        Policy::default(),
        Policy::new(true, Networks::default()),
        Policy::new(
            false,
            Networks::parse("test private allowlist", "10.0.0.0/8").unwrap(),
        ),
        general_policy(),
    ] {
        let selected = export_http_policy(&url("https", "localhost"), general.clone());
        for &address in LOOPBACK.iter().chain(PUBLIC).chain(PRIVATE).chain(REFUSED) {
            assert_eq!(
                selected.allows(ip(address)),
                general.allows(ip(address)),
                "{address}"
            );
            let literal = url("https", address);
            let literal_policy = export_http_policy(&literal, general.clone());
            assert_eq!(
                check_url(&literal_policy, Purpose::Telemetry, &literal).is_ok(),
                check_url(&general, Purpose::Telemetry, &literal).is_ok(),
                "HTTPS literal {address}"
            );
            assert_eq!(
                filter_for_test(
                    &selected,
                    Purpose::Telemetry,
                    "localhost",
                    sockets(&[address])
                )
                .ok(),
                filter_for_test(
                    &general,
                    Purpose::Telemetry,
                    "localhost",
                    sockets(&[address])
                )
                .ok(),
                "HTTPS resolved {address}"
            );
        }
        for answers in [
            vec![],
            vec!["127.0.0.1", "8.8.8.8", "10.1.2.3", "169.254.169.254"],
        ] {
            assert_eq!(
                filter_for_test(
                    &selected,
                    Purpose::Telemetry,
                    "localhost",
                    sockets(&answers)
                )
                .ok(),
                filter_for_test(&general, Purpose::Telemetry, "localhost", sockets(&answers)).ok(),
                "HTTPS must retain general policy's mixed/empty-answer behavior"
            );
        }
    }
}

#[tokio::test]
async fn issue_313_real_http_client_posts_once_to_localhost_and_accepts_bounded_ack() {
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    const BODY: &[u8] = br#"{"resourceSpans":[]}"#;
    const ACK: &[u8] = br#"{"partialSuccess":{"rejectedSpans":"0"}}"#;
    const EXCHANGE: Duration = Duration::from_secs(2);

    // Both futures and all sockets are owned by this bounded scope. No spawned
    // collector task survives a failure or timeout, and no host/env override is
    // used: localhost goes through the actual export client's DNS resolver.
    tokio::time::timeout(Duration::from_secs(5), async {
        let listener = loop {
            let candidate = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
                .await
                .unwrap();
            if ![54339, 54349, 54369].contains(&candidate.local_addr().unwrap().port()) {
                break candidate;
            }
        };
        let port = listener.local_addr().unwrap().port();
        let url = reqwest::Url::parse(&format!("http://localhost:{port}/v1/traces")).unwrap();
        let client = super::export_http_client(
            &url,
            Purpose::Telemetry,
            true,
            Duration::from_secs(1),
            EXCHANGE,
            true,
        )
        .expect("development localhost export client must construct");
        let headers = super::parse_otlp_headers(
            "synthetic test headers",
            "Authorization=Bearer synthetic-issue-313",
        )
        .unwrap();

        let collector = async {
            let (mut stream, peer) = listener.accept().await.unwrap();
            assert!(peer.ip().is_loopback());
            // Read only through the header delimiter, with a strict size cap;
            // the fixed body below cannot be mistaken for another request.
            let mut head = Vec::new();
            while !head.ends_with(b"\r\n\r\n") {
                assert!(head.len() < 8192, "collector request headers exceeded cap");
                head.push(stream.read_u8().await.unwrap());
            }
            let text = std::str::from_utf8(&head).unwrap();
            let mut lines = text.split("\r\n");
            assert_eq!(lines.next(), Some("POST /v1/traces HTTP/1.1"));
            let mut received = reqwest::header::HeaderMap::new();
            for line in lines.take_while(|line| !line.is_empty()) {
                let (name, value) = line.split_once(':').unwrap();
                received.append(
                    reqwest::header::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                    reqwest::header::HeaderValue::from_str(value.trim()).unwrap(),
                );
            }
            for (name, expected) in [
                ("host", format!("localhost:{port}")),
                ("content-type", "application/json".to_owned()),
                ("content-length", BODY.len().to_string()),
                ("authorization", "Bearer synthetic-issue-313".to_owned()),
            ] {
                assert_eq!(received.get_all(name).iter().count(), 1, "{name}");
                assert_eq!(received[name].to_str().unwrap(), expected, "{name}");
            }
            assert!(!received.contains_key("transfer-encoding"));
            let mut body = vec![0; BODY.len()];
            stream.read_exact(&mut body).await.unwrap();
            assert_eq!(body, BODY);

            // A small, complete OTLP acknowledgement exercises post's bounded
            // response reader, not just a successful TCP handshake/status.
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                ACK.len()
            );
            stream.write_all(response.as_bytes()).await.unwrap();
            stream.write_all(ACK).await.unwrap();
            stream.shutdown().await.unwrap();
            let mut extra = [0];
            assert_eq!(stream.read(&mut extra).await.unwrap(), 0, "extra request bytes");
        };
        let delivery = async {
            let result = crate::otlp::post(
                &client,
                &url,
                &headers,
                BODY.to_vec(),
                1,
                "rejectedSpans",
                EXCHANGE,
            )
            .await;
            drop(client);
            result
        };
        let ((), result) = tokio::join!(collector, delivery);
        assert_eq!(result, Ok(0), "collector must accept the synthetic export");
        // The exchange has finished: neither a second connection nor a second
        // request on the first connection may have been emitted.
        assert!(
            tokio::time::timeout(Duration::from_millis(100), listener.accept())
                .await
                .is_err(),
            "export emitted an extra collector connection"
        );
    })
    .await
    .expect("localhost export and collector cleanup must finish within five seconds");
}

#[test]
fn issue_313_real_http_client_constructor_refuses_public_literal_before_transport() {
    use std::time::Duration;
    // #352: refuses a Purpose::Telemetry destination; see
    // `log_capture_test`'s doc comment.
    let _lock = crate::log_capture_test::exclusive();

    // Construction only: never send, resolve, or connect to this public IP,
    // even if the regression returns a client instead of a policy refusal.
    let error = super::export_http_client(
        &url("http", "8.8.8.8"),
        Purpose::Telemetry,
        true,
        Duration::from_secs(1),
        Duration::from_secs(2),
        true,
    )
    .expect_err("HTTP public literal must fail during client construction");
    let reason = error.to_string();
    assert!(reason.contains("telemetry URL refused"));
    assert!(reason.contains("not a loopback address (required for development HTTP)"));
}
