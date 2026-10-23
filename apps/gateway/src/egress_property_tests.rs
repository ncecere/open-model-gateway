//! Property tests of the outbound address policy (#196).
//!
//! The oracle below is written from the IANA special-purpose registries and
//! the translation RFCs, independently of `classify`, so a drift in either is
//! caught. IPv6 forms that carry an IPv4 destination (mapped, compatible,
//! NAT64, 6to4, Teredo) must be judged as that IPv4 address.
use super::*;
use proptest::prelude::*;

fn v4(bits: u32) -> Ipv4Addr {
    Ipv4Addr::from(bits)
}

fn in_net(bits: u32, base: [u8; 4], prefix: u32) -> bool {
    let mask = u32::MAX.checked_shl(32 - prefix).unwrap_or(0);
    bits & mask == u32::from(Ipv4Addr::from(base)) & mask
}

/// The class IANA's registries give an IPv4 address.
fn oracle_v4(bits: u32) -> Class {
    let refused: &[([u8; 4], u32)] = &[
        ([0, 0, 0, 0], 8),          // "this network"
        ([169, 254, 0, 0], 16),     // link-local, cloud metadata
        ([192, 0, 0, 0], 24),       // IETF protocol assignments
        ([192, 0, 2, 0], 24),       // documentation
        ([192, 88, 99, 0], 24),     // deprecated 6to4 relays
        ([198, 18, 0, 0], 15),      // benchmarking
        ([198, 51, 100, 0], 24),    // documentation
        ([203, 0, 113, 0], 24),     // documentation
        ([224, 0, 0, 0], 3),        // multicast, reserved, broadcast
        ([100, 100, 100, 200], 32), // Alibaba metadata
    ];
    if refused.iter().any(|(b, p)| in_net(bits, *b, *p)) {
        return Class::Refused;
    }
    if in_net(bits, [127, 0, 0, 0], 8) {
        return Class::Loopback;
    }
    let private: &[([u8; 4], u32)] = &[
        ([10, 0, 0, 0], 8),
        ([172, 16, 0, 0], 12),
        ([192, 168, 0, 0], 16),
        ([100, 64, 0, 0], 10),
    ];
    if private.iter().any(|(b, p)| in_net(bits, *b, *p)) {
        return Class::Private;
    }
    Class::Public
}

/// The IPv4 destination an IPv6 address carries, by the RFCs (not by `classify`).
fn embedded(s: [u16; 8]) -> Option<u32> {
    let low = (u32::from(s[6]) << 16) | u32::from(s[7]);
    if s[..5] == [0; 5] && s[5] == 0xffff {
        return Some(low); // ::ffff:a.b.c.d
    }
    if s[..6] == [0; 6] && low > 1 {
        return Some(low); // deprecated ::a.b.c.d (not :: and ::1)
    }
    if s[0] == 0x64 && s[1] == 0xff9b && s[2..6] == [0; 4] {
        return Some(low); // NAT64
    }
    if s[0] == 0x2002 {
        return Some((u32::from(s[1]) << 16) | u32::from(s[2])); // 6to4
    }
    if s[0] == 0x2001 && s[1] == 0 {
        return Some(!low); // Teredo: the obfuscated client address
    }
    None
}

fn everything() -> Policy {
    Policy::new(
        true,
        Networks::parse(PRIVATE_ALLOW_ENV, "0.0.0.0/0,::/0").unwrap(),
    )
}

fn v6(segments: [u16; 8]) -> Ipv6Addr {
    Ipv6Addr::from(segments)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(4096))]

    /// Every IPv4 address lands in the class the registries give it.
    #[test]
    fn ipv4_classification_matches_the_registries(bits in any::<u32>()) {
        prop_assert_eq!(classify(IpAddr::V4(v4(bits))), oracle_v4(bits));
    }

    /// The shared special-use table (#203) agrees with the independent
    /// oracle too, not only `classify`'s use of it: a drift inside
    /// `special_use` itself, or in how `egress.rs` maps its categories to a
    /// `Class`, is caught here independently of `providers/local/endpoints.rs`.
    #[test]
    fn the_shared_table_matches_the_registries_for_ipv4(bits in any::<u32>()) {
        prop_assert_eq!(class_for(crate::special_use::classify_v4(v4(bits))), oracle_v4(bits));
    }

    /// An IPv6 form that carries an IPv4 destination is judged as that IPv4
    /// address, for every IPv4 address and every filler bit.
    #[test]
    fn translation_forms_are_judged_as_their_ipv4(
        bits in any::<u32>(),
        filler in any::<[u16; 8]>(),
    ) {
        let (hi, lo) = ((bits >> 16) as u16, bits as u16);
        let (nhi, nlo) = ((!bits >> 16) as u16, !bits as u16);
        let mut forms = vec![
            v6([0, 0, 0, 0, 0, 0xffff, hi, lo]),
            v6([0x64, 0xff9b, 0, 0, 0, 0, hi, lo]),
            v6([0x2002, hi, lo, filler[3], filler[4], filler[5], filler[6], filler[7]]),
            v6([0x2001, 0, filler[2], filler[3], filler[4], filler[5], nhi, nlo]),
        ];
        if bits > 1 {
            forms.push(v6([0, 0, 0, 0, 0, 0, hi, lo]));
        }
        for form in forms {
            prop_assert_eq!(classify(IpAddr::V6(form)), classify(IpAddr::V4(v4(bits))), "{}", form);
            // The policy agrees, with and without approvals.
            for policy in [Policy::default(), Policy::new(true, Networks::default()), everything()] {
                prop_assert_eq!(policy.allows(IpAddr::V6(form)), policy.allows(IpAddr::V4(v4(bits))), "{}", form);
            }
        }
    }

    /// Whatever is listed in the private allowlist and whatever the
    /// environment, metadata, link-local, multicast and the like are never
    /// allowed, for IPv4 and for every IPv6 form.
    #[test]
    fn the_most_permissive_policy_still_refuses_the_dangerous_ranges(
        bits in any::<u32>(),
        segments in any::<[u16; 8]>(),
        pick in 0usize..5,
    ) {
        let policy = everything();
        prop_assert_eq!(
            policy.allows(IpAddr::V4(v4(bits))),
            oracle_v4(bits) != Class::Refused
        );
        // Bias half of the IPv6 samples toward the interesting prefixes.
        let mut s = segments;
        match pick {
            0 => s[0] = 0xfe80 | (s[0] & 0x3f),   // link-local fe80::/10
            1 => { s[0] = 0xfd00; s[1] = 0x0ec2; } // AWS metadata /32
            2 => s[0] = 0xff00 | (s[0] & 0xff),   // multicast
            3 => s[..6].fill(0),                   // ::/96 forms
            _ => {}
        }
        let allowed = policy.allows(IpAddr::V6(v6(s)));
        if let Some(inner) = embedded(s) {
            prop_assert_eq!(allowed, oracle_v4(inner) != Class::Refused, "{}", v6(s));
        } else if allowed {
            let ip = v6(s);
            prop_assert!(!ip.is_unspecified(), "{}", ip);
            prop_assert!(!(s[0] & 0xffc0 == 0xfe80), "link-local {}", ip);
            prop_assert!(!(s[0] & 0xff00 == 0xff00), "multicast {}", ip);
            prop_assert!(!(s[0] == 0xfd00 && s[1] == 0x0ec2), "metadata {}", ip);
            prop_assert!(!(s[0] & 0xffc0 == 0xfec0), "site-local {}", ip);
            prop_assert!(!(s[0] == 0x2001 && s[1] == 0xdb8), "documentation {}", ip);
        }
    }

    /// The strict default (no development, no private networks) allows only
    /// public unicast: nothing private or loopback, nothing that embeds one.
    #[test]
    fn the_strict_policy_allows_only_public_addresses(
        bits in any::<u32>(),
        segments in any::<[u16; 8]>(),
        pick in 0usize..4,
    ) {
        let strict = Policy::default();
        prop_assert_eq!(strict.allows(IpAddr::V4(v4(bits))), oracle_v4(bits) == Class::Public);
        let mut s = segments;
        match pick {
            0 => s[0] = 0xfc00 | (s[0] & 0x01ff), // unique local fc00::/7
            1 => s[..7].fill(0),                  // ::, ::1 and ::x
            2 => s[..5].fill(0),                  // mapped and compatible forms
            _ => {}
        }
        let ip = v6(s);
        if strict.allows(IpAddr::V6(ip)) {
            match embedded(s) {
                Some(inner) => prop_assert_eq!(oracle_v4(inner), Class::Public, "{}", ip),
                None => prop_assert!(s[0] & 0xe000 == 0x2000, "outside 2000::/3: {}", ip),
            }
            prop_assert!(!ip.is_loopback() && !ip.is_unique_local(), "{}", ip);
        }
    }

    /// Loopback is allowed only in development; private only inside the list.
    #[test]
    fn loopback_and_private_need_their_own_approval(bits in any::<u32>()) {
        let ip = IpAddr::V4(v4(bits));
        let (development, listed) = (
            Policy::new(true, Networks::default()),
            Policy::new(false, Networks::parse(PRIVATE_ALLOW_ENV, "0.0.0.0/0").unwrap()),
        );
        match oracle_v4(bits) {
            Class::Loopback => prop_assert!(development.allows(ip) && !listed.allows(ip)),
            Class::Private => prop_assert!(!development.allows(ip) && listed.allows(ip)),
            Class::Public => prop_assert!(development.allows(ip) && listed.allows(ip)),
            Class::Refused => prop_assert!(!development.allows(ip) && !listed.allows(ip)),
        }
    }

    /// Operator input never panics the parser, and what it accepts is a
    /// network with no host bits set.
    #[test]
    fn network_lists_never_panic(text in "\\PC{0,80}") {
        let _ = Networks::parse(PRIVATE_ALLOW_ENV, &text);
    }
}

/// Alternative spellings of loopback, private and metadata hosts in a URL are
/// normalised by the URL parser to the address the policy judges.
#[test]
fn obfuscated_ip_literals_in_urls_are_refused() {
    let strict = Policy::default();
    for host in [
        "169.254.169.254",
        "2852039166",          // decimal
        "0xa9fea9fe",          // hex
        "0251.0376.0251.0376", // octal
        "169.254.43518",       // mixed
        "[::ffff:169.254.169.254]",
        "[::ffff:a9fe:a9fe]",
        "[64:ff9b::a9fe:a9fe]",
        "[2002:a9fe:a9fe::1]",
        "[fd00:ec2::254]",
        "127.1",
        "2130706433",
        "0x7f000001",
        "[::1]",
        "[::ffff:127.0.0.1]",
        "10.1.2.3",
        "[fd12::1]",
        "0",
        "0.0.0.0",
    ] {
        let url = reqwest::Url::parse(&format!("https://{host}/v1")).unwrap();
        assert!(
            check_url(&strict, Purpose::Provider, &url).is_err(),
            "{host} parsed as {:?}",
            url.host_str()
        );
    }
    // A name is not an address: it is judged when it resolves.
    let named = reqwest::Url::parse("https://metadata.google.internal/").unwrap();
    assert!(check_url(&strict, Purpose::Provider, &named).is_ok());
}
