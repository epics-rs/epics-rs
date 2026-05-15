//! EPICS_CAS_* address-list parsing and broadcast-interface discovery.
//!
//! Mirrors the behaviour of `addAddrToChannelAccessAddressList` in
//! `epics-base/modules/database/src/ioc/rsrv/caservertask.c`, providing
//! parsed address lists for the IOC's UDP search responder and beacon
//! emitter.

use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4, ToSocketAddrs};
use std::time::Duration;

use crate::protocol::CA_REPEATER_PORT;

/// Configuration for the CA server's UDP layer.
#[derive(Debug, Clone)]
pub struct CasUdpConfig {
    /// Interfaces (or 0.0.0.0) to bind UDP search responders on.
    pub intf_addrs: Vec<Ipv4Addr>,
    /// Destinations to send beacons to.
    pub beacon_addrs: Vec<SocketAddr>,
    /// Source addresses whose UDP packets should be ignored.
    pub ignore_addrs: Vec<Ipv4Addr>,
    /// Steady-state beacon interval (post-ramp).
    pub beacon_period: Duration,
}

impl Default for CasUdpConfig {
    fn default() -> Self {
        Self {
            intf_addrs: vec![Ipv4Addr::UNSPECIFIED],
            beacon_addrs: vec![SocketAddr::V4(SocketAddrV4::new(
                Ipv4Addr::BROADCAST,
                CA_REPEATER_PORT,
            ))],
            ignore_addrs: Vec::new(),
            beacon_period: Duration::from_secs(15),
        }
    }
}

/// Parse all EPICS_CAS_* environment variables and return a complete
/// UDP configuration. Falls back to sensible defaults (single 0.0.0.0
/// interface, broadcast-only beacon, 15s period) when nothing is set.
pub fn from_env() -> CasUdpConfig {
    let mut cfg = CasUdpConfig::default();

    if let Some(list) = epics_base_rs::runtime::env::get("EPICS_CAS_INTF_ADDR_LIST") {
        let parsed = parse_ipv4_list(&list);
        if !parsed.is_empty() {
            cfg.intf_addrs = parsed;
        }
    }

    // Server-side beacon port: EPICS_CAS_BEACON_PORT takes precedence
    // (matches rsrv/caservertask.c:501-507 lookup order). Falls back to
    // EPICS_CA_REPEATER_PORT, then the compiled-in default. Operators
    // who only set the server-side variable were previously seeing it
    // silently ignored — beacons went to the repeater port.
    let beacon_port = epics_base_rs::runtime::env::get("EPICS_CAS_BEACON_PORT")
        .and_then(|s| s.parse::<u16>().ok())
        .or_else(|| {
            epics_base_rs::runtime::env::get("EPICS_CA_REPEATER_PORT")
                .and_then(|s| s.parse::<u16>().ok())
        })
        .unwrap_or(CA_REPEATER_PORT);

    // Beacon addr list: only EPICS_CAS_BEACON_ADDR_LIST. The C IOC
    // server (rsrv/caservertask.c:413) calls
    // `addAddrToChannelAccessAddressList ( &temp,
    //   &EPICS_CAS_BEACON_ADDR_LIST, ca_beacon_port, 0 )` with no
    // fallback. The fallback to EPICS_CA_ADDR_LIST was intentionally
    // removed in EPICS 3.15 (documentation/RELEASE-3.15.md): "CA
    // servers (RSRV and PCAS) would build the beacon address list
    // using EPICS_CA_ADDR_LIST if EPICS_CAS_BEACON_ADDR_LIST was no
    // set. This is no longer done. Sites depending on this should set
    // both environment variables to the same value." The previous
    // Rust behaviour silently re-enabled the deprecated fallback,
    // sending beacons to every search target on the client list —
    // unwanted UDP fan-out on sites that intentionally separated
    // client search targets from beacon destinations. Note: the
    // standalone `caRepeater` daemon (repeater.cpp:545-547) DOES still
    // fall back; that path lives in `repeater.rs` and is unaffected.
    let mut beacon_addrs: Vec<SocketAddr> = Vec::new();
    if let Some(list) = epics_base_rs::runtime::env::get("EPICS_CAS_BEACON_ADDR_LIST") {
        beacon_addrs.extend(parse_addr_list(&list, beacon_port));
    }

    let auto_beacon = epics_base_rs::runtime::env::get_or("EPICS_CAS_AUTO_BEACON_ADDR_LIST", "YES");
    if auto_beacon.eq_ignore_ascii_case("YES") || beacon_addrs.is_empty() {
        for bcast in discover_broadcast_addrs() {
            let entry = SocketAddr::V4(SocketAddrV4::new(bcast, beacon_port));
            if !beacon_addrs.contains(&entry) {
                beacon_addrs.push(entry);
            }
        }
        if beacon_addrs.is_empty() {
            // Last-resort fallback: limited broadcast.
            beacon_addrs.push(SocketAddr::V4(SocketAddrV4::new(
                Ipv4Addr::BROADCAST,
                beacon_port,
            )));
        }
    }
    cfg.beacon_addrs = beacon_addrs;

    if let Some(list) = epics_base_rs::runtime::env::get("EPICS_CAS_IGNORE_ADDR_LIST") {
        cfg.ignore_addrs = parse_ipv4_list(&list);
    }

    // C `online_notify.c::rsrv_online_notify_task:52-57` reads
    // `EPICS_CAS_BEACON_PERIOD` and falls back to the deprecated
    // `EPICS_CA_BEACON_PERIOD` if the server-side var is unset. The
    // legacy var is still declared in libcom `envDefs.h:62` as
    // "deprecated" precisely because old operator deployments rely on
    // it. Honour the same fallback so a site migrating from a C IOC
    // doesn't silently revert to the default 15s when only the legacy
    // var is in their environment.
    let raw_period = epics_base_rs::runtime::env::get("EPICS_CAS_BEACON_PERIOD")
        .or_else(|| epics_base_rs::runtime::env::get("EPICS_CA_BEACON_PERIOD"));
    if let Some(period) = raw_period.and_then(|s| s.parse::<f64>().ok()) {
        let secs = period.max(0.1);
        cfg.beacon_period = Duration::from_secs_f64(secs);
    }

    cfg
}

/// Parse a whitespace-separated list of "host" or "host:port" tokens.
/// Resolves DNS names if necessary. Unparseable entries are dropped.
pub fn parse_addr_list(list: &str, default_port: u16) -> Vec<SocketAddr> {
    let mut out = Vec::new();
    for token in list.split_whitespace() {
        if let Some(addr) = resolve_token(token, default_port) {
            out.push(addr);
        }
    }
    out
}

fn resolve_token(token: &str, default_port: u16) -> Option<SocketAddr> {
    if let Ok(addr) = token.parse::<SocketAddr>() {
        return Some(addr);
    }
    if let Ok(ip) = token.parse::<Ipv4Addr>() {
        return Some(SocketAddr::V4(SocketAddrV4::new(ip, default_port)));
    }
    let (host, port) = match token.rsplit_once(':') {
        Some((h, p)) => (h, p.parse::<u16>().ok()?),
        None => (token, default_port),
    };
    let candidates = format!("{host}:{port}").to_socket_addrs().ok()?;
    candidates.into_iter().find(|a| a.is_ipv4())
}

/// Parse a whitespace-separated list of IPv4 literals (no port).
fn parse_ipv4_list(list: &str) -> Vec<Ipv4Addr> {
    list.split_whitespace()
        .filter_map(|tok| {
            // Accept "ip" or "ip:port" (port ignored for ignore-list).
            let (host, _) = tok.rsplit_once(':').unwrap_or((tok, ""));
            host.parse::<Ipv4Addr>().ok().or_else(|| {
                // Try DNS as a courtesy.
                format!("{tok}:0")
                    .to_socket_addrs()
                    .ok()?
                    .find_map(|sa| match sa {
                        SocketAddr::V4(v4) => Some(*v4.ip()),
                        _ => None,
                    })
            })
        })
        .collect()
}

/// Discover IPv4 broadcast addresses for all up, non-loopback interfaces.
/// Returns an empty vec if interface enumeration fails (e.g. unsupported OS).
pub fn discover_broadcast_addrs() -> Vec<Ipv4Addr> {
    let mut out = Vec::new();
    let Ok(ifs) = if_addrs::get_if_addrs() else {
        return out;
    };
    for iface in ifs {
        if iface.is_loopback() {
            continue;
        }
        let IpAddr::V4(_v4) = iface.ip() else {
            continue;
        };
        if let if_addrs::IfAddr::V4(v4) = iface.addr {
            if let Some(b) = v4.broadcast {
                // Skip degenerate 0.0.0.0 broadcasts (matches libca
                // osdNetIfAddrs.c osiSockDiscoverBroadcastAddresses, which
                // discards interfaces whose broadcast is INADDR_ANY).
                if b.is_unspecified() {
                    continue;
                }
                if !out.contains(&b) {
                    out.push(b);
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_addr_list_with_ports() {
        let parsed = parse_addr_list("10.0.0.1 192.168.1.255:5066", 5065);
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].port(), 5065);
        assert_eq!(parsed[1].port(), 5066);
    }

    #[test]
    fn parse_ipv4_list_drops_garbage() {
        let v = parse_ipv4_list("1.2.3.4 not-an-ip 5.6.7.8");
        assert_eq!(
            v,
            vec![Ipv4Addr::new(1, 2, 3, 4), Ipv4Addr::new(5, 6, 7, 8)]
        );
    }

    #[test]
    fn empty_list_returns_empty() {
        assert!(parse_addr_list("", 5065).is_empty());
        assert!(parse_ipv4_list("   ").is_empty());
    }

    /// `from_env` MUST NOT fall back to `EPICS_CA_ADDR_LIST` for the
    /// IOC beacon list. C IOC `rsrv/caservertask.c:413` removed that
    /// fallback in EPICS 3.15 (RELEASE-3.15.md). Sites now must set
    /// both env vars; Rust no longer silently re-enables the
    /// deprecated path. The standalone caRepeater (`repeater.rs`)
    /// path keeps the fallback for documented parity with
    /// `epics-base/modules/ca/src/client/repeater.cpp:545-547`.
    #[test]
    #[serial_test::serial]
    fn from_env_does_not_fall_back_to_ca_addr_list() {
        let saved_beacon = std::env::var("EPICS_CAS_BEACON_ADDR_LIST").ok();
        let saved_ca = std::env::var("EPICS_CA_ADDR_LIST").ok();
        let saved_auto = std::env::var("EPICS_CAS_AUTO_BEACON_ADDR_LIST").ok();
        // SAFETY: gated by `serial_test::serial`; mutations confined
        // to this test, restored before return.
        unsafe {
            std::env::remove_var("EPICS_CAS_BEACON_ADDR_LIST");
            std::env::set_var("EPICS_CA_ADDR_LIST", "203.0.113.42:5070");
            // Disable auto-discovery so the result is deterministic
            // (no host broadcast addrs creeping in).
            std::env::set_var("EPICS_CAS_AUTO_BEACON_ADDR_LIST", "NO");
        }

        let cfg = from_env();
        let leaked = cfg
            .beacon_addrs
            .iter()
            .any(|a| matches!(a, SocketAddr::V4(v4) if v4.ip().octets() == [203, 0, 113, 42]));
        assert!(
            !leaked,
            "EPICS_CA_ADDR_LIST entry leaked into beacon_addrs: {:?}",
            cfg.beacon_addrs
        );

        // SAFETY: same scoping as above.
        unsafe {
            match saved_beacon {
                Some(v) => std::env::set_var("EPICS_CAS_BEACON_ADDR_LIST", v),
                None => std::env::remove_var("EPICS_CAS_BEACON_ADDR_LIST"),
            }
            match saved_ca {
                Some(v) => std::env::set_var("EPICS_CA_ADDR_LIST", v),
                None => std::env::remove_var("EPICS_CA_ADDR_LIST"),
            }
            match saved_auto {
                Some(v) => std::env::set_var("EPICS_CAS_AUTO_BEACON_ADDR_LIST", v),
                None => std::env::remove_var("EPICS_CAS_AUTO_BEACON_ADDR_LIST"),
            }
        }
    }

    /// When `EPICS_CAS_BEACON_ADDR_LIST` is set, its entries appear.
    /// Companion assertion to confirm the env-reading branch still
    /// works after removing the fallback.
    #[test]
    #[serial_test::serial]
    fn from_env_uses_beacon_addr_list_when_set() {
        let saved_beacon = std::env::var("EPICS_CAS_BEACON_ADDR_LIST").ok();
        let saved_auto = std::env::var("EPICS_CAS_AUTO_BEACON_ADDR_LIST").ok();
        // SAFETY: serial_test::serial.
        unsafe {
            std::env::set_var("EPICS_CAS_BEACON_ADDR_LIST", "198.51.100.7:5099");
            std::env::set_var("EPICS_CAS_AUTO_BEACON_ADDR_LIST", "NO");
        }

        let cfg = from_env();
        let hit = cfg.beacon_addrs.iter().any(|a| {
            matches!(a, SocketAddr::V4(v4)
                if v4.ip().octets() == [198, 51, 100, 7] && v4.port() == 5099)
        });
        assert!(
            hit,
            "EPICS_CAS_BEACON_ADDR_LIST entry missing from beacon_addrs: {:?}",
            cfg.beacon_addrs
        );

        // SAFETY: same scoping as above.
        unsafe {
            match saved_beacon {
                Some(v) => std::env::set_var("EPICS_CAS_BEACON_ADDR_LIST", v),
                None => std::env::remove_var("EPICS_CAS_BEACON_ADDR_LIST"),
            }
            match saved_auto {
                Some(v) => std::env::set_var("EPICS_CAS_AUTO_BEACON_ADDR_LIST", v),
                None => std::env::remove_var("EPICS_CAS_AUTO_BEACON_ADDR_LIST"),
            }
        }
    }
}
