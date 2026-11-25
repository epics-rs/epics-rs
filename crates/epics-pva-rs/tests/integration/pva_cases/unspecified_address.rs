//! Wire-shape: 16-byte unspecified address decode.
//!
//! pvxs `src/evhelper.cpp:911-938` decodes the all-zero 16-byte
//! address as IPv6 unspecified (`SockAddr::isAny()` true).
//! Downstream code (`src/client.cpp:841-843`,
//! `src/udp_collector.cpp:471-476`) then substitutes the UDP
//! source for any SEARCH_RESPONSE / BEACON carrying the
//! wildcard. Pre-fix Rust's `ip_from_bytes` returned `None` —
//! which the search engine dropped silently, leaving any IPv6
//! wildcard-advertising peer invisible to a Rust client.
//!
//! These tests don't exercise an encoder per se — they pin the
//! decoder contract that the unspecified-address decode depends on.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use epics_pva_rs::proto::{ip_from_bytes, ip_from_bytes_allow_unspec};

#[test]
fn golden_pvxs_ipv4_mapped_round_trip() {
    let bytes = [
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, //
        0x00, 0x00, 0xff, 0xff, 192, 168, 1, 100,
    ];
    assert_eq!(
        ip_from_bytes(&bytes),
        Some(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 100)))
    );
}

#[test]
fn golden_pvxs_ipv6_unspecified_is_none_via_strict() {
    // Strict ip_from_bytes treats all-zero as "no address" —
    // legacy behaviour preserved for callers that want it.
    assert_eq!(ip_from_bytes(&[0u8; 16]), None);
}

#[test]
fn golden_pvxs_ipv6_unspecified_is_wildcard_via_allow_unspec() {
    // Helper: returns IPv6 :: so caller can apply
    // pvxs-style UDP-source substitution.
    let ip = ip_from_bytes_allow_unspec(&[0u8; 16]);
    assert_eq!(ip, IpAddr::V6(Ipv6Addr::UNSPECIFIED));
    assert!(ip.is_unspecified(), "must satisfy IpAddr::is_unspecified()");
}

#[test]
fn golden_pvxs_ipv4_unspecified_via_allow_unspec() {
    // The 0.0.0.0 wire form (IPv4-mapped all-zero) → IPv4 0.0.0.0.
    let bytes = [
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, //
        0x00, 0x00, 0xff, 0xff, 0x00, 0x00, 0x00, 0x00,
    ];
    let ip = ip_from_bytes_allow_unspec(&bytes);
    assert_eq!(ip, IpAddr::V4(Ipv4Addr::UNSPECIFIED));
    assert!(ip.is_unspecified());
}
