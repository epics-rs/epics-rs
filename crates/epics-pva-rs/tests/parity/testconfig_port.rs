//! Port of pvxs's `test/testconfig.cpp::testParse`.
//!
//! pvxs verifies that `EPICS_PVA_ADDR_LIST` is parsed into a list of
//! `SocketAddr`, with the default port supplied from
//! `EPICS_PVA_BROADCAST_PORT` (or 5076). Plain IP entries get the
//! default port appended; `host:port` entries keep their explicit
//! port. Whitespace and surrounding spaces are tolerated.

#![cfg(test)]

use std::net::SocketAddr;
use std::time::Duration;

use epics_pva_rs::client_native::search::parse_addr_list;
use epics_pva_rs::config::env;

#[test]
fn pvxs_parse_addr_list_two_entries_explicit_default() {
    // pvxs sets EPICS_PVA_BROADCAST_PORT=1234 then parses
    //   "  1.2.3.4  5.6.7.8:9876  "
    // expecting ["1.2.3.4:1234", "5.6.7.8:9876"].
    //
    // We can't safely mutate process env in parallel tests, so we pin
    // the default port by passing through a raw string and asserting
    // the explicit-port entry survives. The default-port case is
    // covered by pvxs_parse_addr_list_default_port_substituted below.
    let addrs = parse_addr_list("  1.2.3.4  5.6.7.8:9876  ");
    assert_eq!(addrs.len(), 2);
    // First entry should keep IP and pick up *some* default port (5076 or env).
    assert_eq!(format!("{}", addrs[0].ip()), "1.2.3.4");
    // Second entry has explicit port.
    let want: SocketAddr = "5.6.7.8:9876".parse().unwrap();
    assert_eq!(addrs[1], want);
}

#[test]
fn pvxs_parse_addr_list_default_port_substituted() {
    // Without EPICS_PVA_BROADCAST_PORT, plain IPs get port 5076.
    let prev = std::env::var("EPICS_PVA_BROADCAST_PORT").ok();
    // Safety: set to "" then check, restore at end. Tests must run serially.
    // SAFETY: Single-threaded test scope; we only touch one env var.
    unsafe {
        std::env::remove_var("EPICS_PVA_BROADCAST_PORT");
    }
    let addrs = parse_addr_list("10.0.0.1");
    assert_eq!(addrs.len(), 1);
    assert_eq!(addrs[0].port(), 5076);
    if let Some(p) = prev {
        unsafe {
            std::env::set_var("EPICS_PVA_BROADCAST_PORT", p);
        }
    }
}

#[test]
fn pvxs_addr_list_comma_is_ttl_modifier_not_a_separator() {
    // pvxs splits EPICS_PVA_ADDR_LIST on WHITESPACE only — `split_addr_into`
    // (config.cpp:159-160) scans with find_first_of(" \t\r\n"). Within one
    // token a comma introduces the multicast TTL, `<IP>,<ttl#>[@iface]`
    // (`SockEndpoint`, config.cpp:32-57); it is NOT a second-address
    // separator. So "1.1.1.1,2.2.2.2" is a single token whose TTL "2.2.2.2"
    // fails to parse as an integer: pvxs throws in the SockEndpoint ctor and
    // split_addr_into drops the whole token (config.cpp:171-174), yielding
    // zero addresses. (The previous assertion of two addresses assumed a
    // comma separator pvxs does not have; the crate's own unit test at
    // client_native/search.rs already pins the same "1.2.3.4,5.6.7.8" input
    // as empty.)
    let addrs = parse_addr_list("1.1.1.1,2.2.2.2");
    assert!(
        addrs.is_empty(),
        "comma is pvxs's multicast-TTL modifier, not an address separator; \
         an invalid TTL drops the whole token (got {addrs:?})"
    );
}

#[test]
fn pvxs_parse_addr_list_empty_yields_empty() {
    let addrs = parse_addr_list("");
    assert!(addrs.is_empty());
}

#[test]
fn pvxs_parse_addr_list_skips_invalid_entries() {
    // Non-parsable entries are silently dropped.
    let addrs = parse_addr_list("garbage 127.0.0.1 also-bad 192.168.1.1:5075");
    assert_eq!(addrs.len(), 2);
    assert_eq!(format!("{}", addrs[0].ip()), "127.0.0.1");
    let want: SocketAddr = "192.168.1.1:5075".parse().unwrap();
    assert_eq!(addrs[1], want);
}

// ── EPICS_PVA{,S}_* multi-NIC env vars (testDefs port) ─────────────

#[test]
fn pvxs_parse_addr_list_with_explicit_default_port() {
    // pvxs: addr_list with default port 1234 → first IP gets 1234,
    // second keeps explicit 9876.
    let addrs = env::parse_addr_list_with_port("1.2.3.4 5.6.7.8:9876", 1234);
    assert_eq!(addrs.len(), 2);
    assert_eq!(addrs[0].port(), 1234);
    assert_eq!(addrs[1].port(), 9876);
}

#[test]
fn pvxs_broadcast_addresses_includes_limited_broadcast() {
    // Even on a host with no usable NIC, list_broadcast_addresses
    // always includes 255.255.255.255 as a fallback.
    let bcasts = env::list_broadcast_addresses(5076);
    assert!(
        bcasts
            .iter()
            .any(|a| { format!("{}", a.ip()) == "255.255.255.255" && a.port() == 5076 })
    );
}

#[test]
fn pvxs_beacon_period_default_is_15_seconds() {
    // When EPICS_PVAS_BEACON_PERIOD is unset, default is 15s.
    let prev = std::env::var("EPICS_PVAS_BEACON_PERIOD").ok();
    unsafe { std::env::remove_var("EPICS_PVAS_BEACON_PERIOD") };
    assert_eq!(env::beacon_period(), Duration::from_secs(15));
    if let Some(v) = prev {
        unsafe { std::env::set_var("EPICS_PVAS_BEACON_PERIOD", v) };
    }
}

/// A fractional beacon period must survive as sub-second precision, not
/// truncate to a zero-second emit-loop. `0.5` → 500ms, `0.05` floors to
/// the 100ms minimum, `1.0` → 1s, an invalid string → the 15s default.
#[test]
fn beacon_period_preserves_subsecond_values() {
    let prev = std::env::var("EPICS_PVAS_BEACON_PERIOD").ok();
    let cases = [
        ("0", Duration::from_secs(15)),       // zero rejected → default
        ("0.05", Duration::from_millis(100)), // below floor → 100ms
        ("0.5", Duration::from_millis(500)),  // honored as 500ms
        ("1.0", Duration::from_secs(1)),
        ("not-a-number", Duration::from_secs(15)), // invalid → default
    ];
    for (val, expect) in cases {
        unsafe { std::env::set_var("EPICS_PVAS_BEACON_PERIOD", val) };
        assert_eq!(
            env::beacon_period(),
            expect,
            "EPICS_PVAS_BEACON_PERIOD={val}"
        );
    }
    match prev {
        Some(v) => unsafe { std::env::set_var("EPICS_PVAS_BEACON_PERIOD", v) },
        None => unsafe { std::env::remove_var("EPICS_PVAS_BEACON_PERIOD") },
    }
}

/// `EPICS_PVAS_BEACON_PERIOD_LONG` likewise keeps sub-second precision and
/// floors at 100ms; absent → `None` (caller derives 12× short).
#[test]
fn beacon_period_long_preserves_subsecond_values() {
    let prev = std::env::var("EPICS_PVAS_BEACON_PERIOD_LONG").ok();
    unsafe { std::env::remove_var("EPICS_PVAS_BEACON_PERIOD_LONG") };
    assert_eq!(env::beacon_period_long(), None);
    unsafe { std::env::set_var("EPICS_PVAS_BEACON_PERIOD_LONG", "0.5") };
    assert_eq!(env::beacon_period_long(), Some(Duration::from_millis(500)));
    unsafe { std::env::set_var("EPICS_PVAS_BEACON_PERIOD_LONG", "0") };
    assert_eq!(env::beacon_period_long(), None); // zero rejected
    match prev {
        Some(v) => unsafe { std::env::set_var("EPICS_PVAS_BEACON_PERIOD_LONG", v) },
        None => unsafe { std::env::remove_var("EPICS_PVAS_BEACON_PERIOD_LONG") },
    }
}

#[test]
fn pvxs_name_servers_empty_when_unset() {
    let prev = std::env::var("EPICS_PVA_NAME_SERVERS").ok();
    unsafe { std::env::remove_var("EPICS_PVA_NAME_SERVERS") };
    assert!(env::name_servers().is_empty());
    if let Some(v) = prev {
        unsafe { std::env::set_var("EPICS_PVA_NAME_SERVERS", v) };
    }
}

#[test]
fn pvxs_conn_tmo_default_is_30_seconds() {
    let prev = std::env::var("EPICS_PVA_CONN_TMO").ok();
    unsafe { std::env::remove_var("EPICS_PVA_CONN_TMO") };
    assert_eq!(env::conn_timeout_secs(), 30.0);
    if let Some(v) = prev {
        unsafe { std::env::set_var("EPICS_PVA_CONN_TMO", v) };
    }
}
