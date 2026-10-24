//! #313: exercise the policy actually selected by export_http_client, using
//! the production literal check and post-DNS filter without network or env I/O.
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
