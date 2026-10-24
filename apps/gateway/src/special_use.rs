//! Shared special-use address classification (#203), used by both the
//! outbound egress policy (`egress.rs`) and the approved local-upstream
//! check (`providers/local/endpoints.rs`) so the two cannot drift on what a
//! given concrete address *is*.
//!
//! This module only classifies: it has no notion of environment, scheme or
//! an operator allowlist. Each caller keeps its own translation step (IPv4
//! mapped/compatible, NAT64, 6to4, Teredo: which address a form should be
//! judged as is a decision specific to each caller's purpose) and its own
//! mapping from a [`Category`] to an accept/refuse decision, because the two
//! callers have legitimately different rules for the same category (for
//! example: `egress.rs` lets a private allowlist reach carrier-grade NAT,
//! while an approved local upstream never may). Everything that is refused
//! for *every* caller is decided once, by [`Category::disposition`], so the
//! two callers cannot drift on which ranges are refused (#203).
//!
//! The table covers IANA's special-purpose address registries (RFC 6890 and
//! its IPv6 counterpart) plus the translation/benchmarking/documentation
//! allocations named in #196 and #203. `None` of these categories match an
//! ordinary public or private unicast address; those are [`Category::Public`]
//! or [`Category::Private`].

use std::net::{Ipv4Addr, Ipv6Addr};

/// What kind of special-use range a concrete IPv4 or (untranslated) IPv6
/// address falls in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Category {
    /// Ordinary unicast: not in any range below.
    Public,
    /// RFC 1918 (IPv4) or unique-local `fc00::/7` (IPv6), apart from the AWS
    /// metadata range carved out below.
    Private,
    /// Loopback: `127.0.0.0/8` or `::1`.
    Loopback,
    /// Link-local, including the IPv4 cloud-metadata address
    /// `169.254.169.254` (`169.254.0.0/16`) and IPv6 `fe80::/10`.
    LinkLocal,
    Multicast,
    /// `255.255.255.255` (IPv4 only; IPv6 has no broadcast).
    Broadcast,
    /// IPv4 `0.0.0.0/8` ("this network") and `240.0.0.0/4`; the deprecated
    /// IPv6 site-local range `fec0::/10`; and, as a safe default, any other
    /// IPv6 address outside `2000::/3` and outside unique-local (this is
    /// where an untranslated NAT64 literal `64:ff9b::/96` lands: #203 keeps
    /// it refused).
    Reserved,
    /// A single-address cloud metadata endpoint that is not already
    /// `LinkLocal`: Alibaba Cloud's `100.100.100.200` (IPv4, inside the CGNAT
    /// range) and the exact AWS IPv6 metadata address `fd00:ec2::254`.
    Metadata,
    /// `100.64.0.0/10` (carrier-grade NAT, also used by Tailscale), IPv4
    /// only, apart from the Alibaba metadata address carved out above.
    Cgnat,
    /// `fd00:ec2::/32` apart from the exact metadata address `fd00:ec2::254`
    /// (IPv6 only): AWS's unique-local block, refused everywhere (#203).
    AwsMetadataRange,
    /// RFC 2544 benchmarking: `198.18.0.0/15` (IPv4), `2001:2::/48` (IPv6).
    Benchmarking,
    /// RFC 5737 / RFC 3849 documentation (`192.0.2.0/24`, `198.51.100.0/24`,
    /// `203.0.113.0/24`, `2001:db8::/32`), plus the IETF-reserved
    /// `3fff::/20` (RFC 9637), grouped with documentation per #203.
    Documentation,
    /// RFC 5736 IETF protocol assignments: `192.0.0.0/24` (IPv4 only).
    IetfProtocolAssignments,
    /// RFC 3068 deprecated 6to4 anycast relay: `192.88.99.0/24` (IPv4 only).
    Deprecated6to4Relay,
}

impl Category {
    /// A short, operator-facing name for a startup error message, never a
    /// secret.
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Public => "public",
            Self::Private => "private",
            Self::Loopback => "loopback",
            Self::LinkLocal => "link-local",
            Self::Multicast => "multicast",
            Self::Broadcast => "broadcast",
            Self::Reserved => "reserved",
            Self::Metadata => "metadata",
            Self::Cgnat => "carrier-grade NAT",
            Self::AwsMetadataRange => "AWS metadata range (fd00:ec2::/32)",
            Self::Benchmarking => "benchmarking",
            Self::Documentation => "documentation",
            Self::IetfProtocolAssignments => "IETF protocol assignments",
            Self::Deprecated6to4Relay => "deprecated 6to4 relay",
        }
    }
}

/// What every caller must do with a [`Category`], before its own settings
/// (a private allowlist, the development flag, the URL scheme) apply.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Disposition {
    /// Never reachable, whatever the configuration: link-local, multicast,
    /// broadcast, reserved, metadata (including the whole AWS `fd00:ec2::/32`),
    /// benchmarking, documentation, IETF protocol assignments and the
    /// deprecated 6to4 relay.
    Refused,
    Loopback,
    /// RFC 1918 and IPv6 unique-local.
    Private,
    /// `100.64.0.0/10`: private for egress (the allowlist decides), never an
    /// approved local upstream.
    Cgnat,
    Public,
}

impl Category {
    /// The single place that says which categories are refused outright.
    /// The match is exhaustive on purpose: a new category must be decided
    /// here, for both callers at once.
    pub(crate) fn disposition(self) -> Disposition {
        match self {
            Self::Public => Disposition::Public,
            Self::Private => Disposition::Private,
            Self::Loopback => Disposition::Loopback,
            Self::Cgnat => Disposition::Cgnat,
            Self::LinkLocal
            | Self::Multicast
            | Self::Broadcast
            | Self::Reserved
            | Self::Metadata
            | Self::AwsMetadataRange
            | Self::Benchmarking
            | Self::Documentation
            | Self::IetfProtocolAssignments
            | Self::Deprecated6to4Relay => Disposition::Refused,
        }
    }
}

/// The exact AWS IPv6 metadata address, `fd00:ec2::254`.
const AWS_METADATA: Ipv6Addr = Ipv6Addr::new(0xfd00, 0x0ec2, 0, 0, 0, 0, 0, 0x0254);

/// Classify a concrete IPv4 address. Order matters only where ranges
/// overlap: the Alibaba metadata address is inside the CGNAT range, so it is
/// checked first.
pub(crate) fn classify_v4(ip: Ipv4Addr) -> Category {
    let o = ip.octets();
    if ip == Ipv4Addr::new(100, 100, 100, 200) {
        return Category::Metadata;
    }
    // Broadcast (255.255.255.255) is inside 240.0.0.0/4: check it first so
    // it keeps its own category rather than falling into `Reserved`
    // (harmless either way: both are always refused).
    if ip.is_broadcast() {
        return Category::Broadcast;
    }
    if ip.is_unspecified() || o[0] == 0 || o[0] >= 240 {
        return Category::Reserved;
    }
    if ip.is_link_local() {
        return Category::LinkLocal;
    }
    if ip.is_multicast() {
        return Category::Multicast;
    }
    if o[0] == 198 && (o[1] == 18 || o[1] == 19) {
        return Category::Benchmarking;
    }
    if o[0] == 192 && o[1] == 0 && o[2] == 0 {
        return Category::IetfProtocolAssignments;
    }
    if o[0] == 192 && o[1] == 88 && o[2] == 99 {
        return Category::Deprecated6to4Relay;
    }
    if ip.is_documentation() {
        return Category::Documentation;
    }
    if ip.is_loopback() {
        return Category::Loopback;
    }
    if o[0] == 100 && (64..=127).contains(&o[1]) {
        return Category::Cgnat;
    }
    if ip.is_private() {
        return Category::Private;
    }
    Category::Public
}

/// Classify a concrete IPv6 address that the caller has already determined
/// is not loopback (`::1`) and does not carry an IPv4 address in a mapped,
/// compatible, NAT64, 6to4 or Teredo form (each caller judges those forms as
/// the IPv4 address they carry, under its own rules for whether and when to
/// do so). `64:ff9b::/96` (NAT64) reaching this function unresolved lands in
/// [`Category::Reserved`] (outside `2000::/3`, not unique-local): refused.
pub(crate) fn classify_v6(ip: Ipv6Addr) -> Category {
    let s = ip.segments();
    if ip == AWS_METADATA {
        return Category::Metadata;
    }
    if s[0] == 0xfd00 && s[1] == 0x0ec2 {
        return Category::AwsMetadataRange;
    }
    if ip.is_unspecified() {
        return Category::Reserved;
    }
    if ip.is_unicast_link_local() {
        return Category::LinkLocal;
    }
    if ip.is_multicast() {
        return Category::Multicast;
    }
    // Deprecated site-local fec0::/10.
    if (s[0] & 0xffc0) == 0xfec0 {
        return Category::Reserved;
    }
    if ip.is_unique_local() {
        return Category::Private;
    }
    if s[0] == 0x2001 && s[1] == 0xdb8 {
        return Category::Documentation;
    }
    // 3fff::/20, IETF-reserved, grouped with documentation. #244 fix: this
    // used to use the wider `(s[0] & 0xfff0) == 0x3ff0` mask, i.e.
    // 3ff0::/12, which incorrectly classified all of 3ff0::/12 as
    // documentation (every address whose first hextet is 0x3ff0..=0x3fff).
    // `s[0]` supplies 16 of the 20 prefix bits; the remaining 4 are the top
    // nibble of `s[1]`, so both must match.
    if s[0] == 0x3fff && (s[1] & 0xf000) == 0 {
        return Category::Documentation;
    }
    if s[0] == 0x2001 && s[1] == 2 && s[2] == 0 {
        return Category::Benchmarking;
    }
    if (s[0] & 0xe000) == 0x2000 {
        return Category::Public;
    }
    Category::Reserved
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v4(s: &str) -> Ipv4Addr {
        s.parse().unwrap()
    }
    fn v6(s: &str) -> Ipv6Addr {
        s.parse().unwrap()
    }

    /// Only these categories are not refused outright; every other one is
    /// refused for every caller (#203). CGNAT is its own disposition because
    /// egress gates it on a private allowlist while approvals never accept it.
    #[test]
    fn disposition_refuses_every_special_use_category() {
        for (category, expected) in [
            (Category::Public, Disposition::Public),
            (Category::Private, Disposition::Private),
            (Category::Loopback, Disposition::Loopback),
            (Category::Cgnat, Disposition::Cgnat),
            (Category::LinkLocal, Disposition::Refused),
            (Category::Multicast, Disposition::Refused),
            (Category::Broadcast, Disposition::Refused),
            (Category::Reserved, Disposition::Refused),
            (Category::Metadata, Disposition::Refused),
            (Category::AwsMetadataRange, Disposition::Refused),
            (Category::Benchmarking, Disposition::Refused),
            (Category::Documentation, Disposition::Refused),
            (Category::IetfProtocolAssignments, Disposition::Refused),
            (Category::Deprecated6to4Relay, Disposition::Refused),
        ] {
            assert_eq!(category.disposition(), expected, "{category:?}");
        }
    }

    #[test]
    fn v4_categories_match_the_named_ranges() {
        let cases = [
            ("8.8.8.8", Category::Public),
            ("10.1.2.3", Category::Private),
            ("172.16.0.1", Category::Private),
            ("192.168.1.1", Category::Private),
            ("127.0.0.1", Category::Loopback),
            ("0.0.0.0", Category::Reserved),
            ("0.1.2.3", Category::Reserved),
            ("255.255.255.255", Category::Broadcast),
            ("224.0.0.1", Category::Multicast),
            ("169.254.169.254", Category::LinkLocal),
            ("169.254.1.1", Category::LinkLocal),
            ("240.0.0.1", Category::Reserved),
            ("100.64.0.1", Category::Cgnat),
            ("100.127.255.254", Category::Cgnat),
            ("100.100.100.200", Category::Metadata),
            ("198.18.0.1", Category::Benchmarking),
            ("198.19.255.254", Category::Benchmarking),
            ("192.0.0.1", Category::IetfProtocolAssignments),
            ("192.88.99.1", Category::Deprecated6to4Relay),
            ("192.0.2.1", Category::Documentation),
            ("198.51.100.1", Category::Documentation),
            ("203.0.113.1", Category::Documentation),
        ];
        for (ip, expected) in cases {
            assert_eq!(classify_v4(v4(ip)), expected, "{ip}");
        }
    }

    #[test]
    fn v6_categories_match_the_named_ranges() {
        let cases = [
            ("2606:4700::1111", Category::Public),
            ("fd12::1", Category::Private),
            ("fe80::1", Category::LinkLocal),
            ("ff02::1", Category::Multicast),
            ("::", Category::Reserved),
            ("fec0::1", Category::Reserved),
            ("64:ff9b::808:808", Category::Reserved),
            ("fd00:ec2::254", Category::Metadata),
            ("fd00:ec2::1", Category::AwsMetadataRange),
            ("fd00:ec2::ffff", Category::AwsMetadataRange),
            ("2001:db8::1", Category::Documentation),
            ("3fff::1", Category::Documentation),
            ("3fff:0fff:ffff::1", Category::Documentation),
            // #244: only `3fff::/20` is documentation; `3fff:ffff::1` has a
            // non-zero top nibble in its second hextet, outside the /20, so
            // it is ordinary global unicast.
            ("3fff:ffff::1", Category::Public),
            ("2001:2::1", Category::Benchmarking),
        ];
        for (ip, expected) in cases {
            assert_eq!(classify_v6(v6(ip)), expected, "{ip}");
        }
    }

    fn refused4(addr: &str) -> bool {
        classify_v4(v4(addr)).disposition() == Disposition::Refused
    }
    fn refused6(addr: &str) -> bool {
        classify_v6(v6(addr)).disposition() == Disposition::Refused
    }

    /// The refused set is exactly `3fff::/20` here: `3fff::1` and the last
    /// address of the `/20` are refused; `3fff:1000::1` (just outside),
    /// `3ff0::1` and `3ffe:ffff::1` are ordinary global unicast.
    #[test]
    fn refused_v6_3fff_boundary_is_exactly_the_20_not_the_legacy_12() {
        for (addr, expected) in [
            ("3fff::1", true),
            ("3fff:0fff:ffff:ffff:ffff:ffff:ffff:ffff", true),
            ("3fff:1000::1", false),
            ("3ff0::1", false),
            ("3ffe:ffff::1", false),
        ] {
            assert_eq!(refused6(addr), expected, "{addr}");
        }
    }

    /// #244: the address just outside `3fff::/20` is `Category::Public`
    /// (ordinary global unicast, like the rest of `2000::/3`), not
    /// `Category::Documentation` as the pre-#244 `3ff0::/12` mask gave it,
    /// and is not refused.
    #[test]
    fn classification_outside_3fff_20_is_public_and_not_refused_244() {
        for addr in ["3ff0::1", "3ffe:ffff::1"] {
            assert_eq!(classify_v6(v6(addr)), Category::Public, "{addr}");
            assert!(!refused6(addr), "{addr}");
        }
    }

    /// First and last address of every IPv4 range the issue lists, and the
    /// address just outside each boundary, as the shared table refuses them.
    #[test]
    fn refused_v4_boundaries_match_the_issue_exactly() {
        let cases: &[(&str, bool)] = &[
            ("198.18.0.0", true),
            ("198.19.255.255", true),
            ("198.17.255.255", false),
            ("198.20.0.0", false),
            ("192.0.0.0", true),
            ("192.0.0.255", true),
            ("191.255.255.255", false),
            ("192.0.1.0", false),
            ("192.88.99.0", true),
            ("192.88.99.255", true),
            ("192.88.98.255", false),
            ("192.88.100.0", false),
            ("192.0.2.0", true),
            ("192.0.2.255", true),
            ("192.0.1.255", false),
            ("192.0.3.0", false),
            ("198.51.100.0", true),
            ("198.51.100.255", true),
            ("198.51.99.255", false),
            ("198.51.101.0", false),
            ("203.0.113.0", true),
            ("203.0.113.255", true),
            ("203.0.112.255", false),
            ("203.0.114.0", false),
        ];
        for (addr, expected) in cases {
            assert_eq!(refused4(addr), *expected, "{addr}");
        }
    }

    /// #244: `classify_v6`'s documentation check must match exactly
    /// `3fff::/20` (RFC 9637), not the wider `3ff0::/12` the buggy
    /// `(s[0] & 0xfff0) == 0x3ff0` mask matched. `3fff::1` and the last
    /// address of `3fff::/20` stay `Documentation`; `3fff:1000::1` (just
    /// outside the `/20`), `3ff0::1`, `3ffe:ffff::1` and `3ff1:dead::1`
    /// (inside the old buggy `/12` mask but outside the `/20`) become
    /// `Public`, ordinary global unicast, like the rest of `2000::/3`.
    #[test]
    fn issue_244_documentation_is_exactly_3fff_20_not_3ff0_12() {
        for (addr, expected) in [
            ("3fff::1", Category::Documentation),
            (
                "3fff:0fff:ffff:ffff:ffff:ffff:ffff:ffff",
                Category::Documentation,
            ),
            ("3fff:1000::1", Category::Public),
            ("3ff0::1", Category::Public),
            ("3ffe:ffff::1", Category::Public),
            ("3ff1:dead::1", Category::Public),
        ] {
            assert_eq!(classify_v6(v6(addr)), expected, "{addr}");
        }
    }

    /// First and last address of every IPv6 range the issue lists (apart
    /// from `3fff::/20`, covered above), and the address just outside each
    /// boundary; plus the exact AWS metadata address, refused too.
    #[test]
    fn refused_v6_boundaries_match_the_issue_exactly() {
        let cases: &[(&str, bool)] = &[
            ("2001:db8::", true),
            ("2001:db8:ffff:ffff:ffff:ffff:ffff:ffff", true),
            ("2001:db7:ffff:ffff:ffff:ffff:ffff:ffff", false),
            ("2001:db9::", false),
            ("2001:2::", true),
            ("2001:2:0:ffff:ffff:ffff:ffff:ffff", true),
            ("2001:1:ffff:ffff:ffff:ffff:ffff:ffff", false),
            ("2001:2:1::", false),
            ("fd00:ec2::", true),
            ("fd00:ec2:ffff:ffff:ffff:ffff:ffff:ffff", true),
            ("fd00:ec2::254", true),
            ("fd00:ec1:ffff:ffff:ffff:ffff:ffff:ffff", false),
            ("fd00:ec3::", false),
        ];
        for (addr, expected) in cases {
            assert_eq!(refused6(addr), *expected, "{addr}");
        }
    }
}
