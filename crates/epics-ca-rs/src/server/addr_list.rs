//! EPICS_CAS_* address-list parsing and broadcast-interface discovery.
//!
//! Mirrors the behaviour of `addAddrToChannelAccessAddressList` in
//! `epics-base/modules/database/src/ioc/rsrv/caservertask.c`, providing
//! parsed address lists for the IOC's UDP search responder and beacon
//! emitter.

use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4, ToSocketAddrs};
use std::time::Duration;

use crate::protocol::CA_REPEATER_PORT;
use epics_base_rs::error::{CaError, CaResult};

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
    /// multicast groups (224.0.0.0/4) extracted from
    /// `EPICS_CAS_INTF_ADDR_LIST`. C `rsrv/caservertask.c:367-371,
    /// 633-668` keeps these in `casMCastAddrList` and joins each
    /// group via `IP_ADD_MEMBERSHIP` from a wildcard-bound socket;
    /// they cannot be unicast-bound.
    pub mcast_addrs: Vec<Ipv4Addr>,
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
            mcast_addrs: Vec::new(),
        }
    }
}

/// Parse all EPICS_CAS_* environment variables and return a complete
/// UDP configuration. Falls back to sensible defaults (single 0.0.0.0
/// interface, broadcast-only beacon, 15s period) when nothing is set.
///
/// returns `Err` when `EPICS_CAS_INTF_ADDR_LIST` mixes
/// `0.0.0.0` with specific interface IPs — C
/// `rsrv/caservertask.c:390-392` `cantProceed`s on this combination
/// (which kills the IOC process). The error propagates to the
/// caller (`CaServer::run` / `run_tcp_listener`) which fails
/// startup, matching C's fatal behaviour. The check runs
/// UNCONDITIONALLY, not just when `EPICS_CAS_AUTO_BEACON_ADDR_LIST=YES`
/// — pre-fix Rust nested it under `if auto_on`, so the misconfig
/// silently escaped detection when AUTO=NO.
pub fn from_env() -> CaResult<CasUdpConfig> {
    let mut cfg = CasUdpConfig::default();

    if let Some(list) = epics_base_rs::runtime::env::get("EPICS_CAS_INTF_ADDR_LIST") {
        let parsed = parse_ipv4_list(&list);
        // C `rsrv/caservertask.c:367-371, 633-668` splits
        // multicast (224.0.0.0/4) entries off into
        // `casMCastAddrList` and joins each group via
        // `IP_ADD_MEMBERSHIP` on a wildcard-bound socket; trying
        // to `bind()` a unicast socket to a multicast IP fails on
        // most kernels. Filter the multicast entries here into
        // `cfg.mcast_addrs` so they don't reach the unicast bind
        // path; the responder side (server/udp.rs) joins them.
        // Without this split the multicast addresses caused
        // silent per-interface bind failures and PVs became
        // invisible to multicast SEARCH topologies.
        let (mcast, unicast): (Vec<_>, Vec<_>) =
            parsed.into_iter().partition(|ip| ip.is_multicast());
        if !unicast.is_empty() {
            cfg.intf_addrs = unicast;
        }
        if !mcast.is_empty() {
            cfg.mcast_addrs = mcast;
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

    // C parity (`caservertask.c:281-287, 415-427`):
    //   * Default `autobeaconlist = 1` (YES). The CONFIG_ENV default
    //     `EPICS_CAS_AUTO_BEACON_ADDR_LIST=""` parses as NULL in
    //     `envGetConfigParamPtr` (empty-string → NULL), so
    //     `envGetBoolConfigParam` returns -1 and `autobeaconlist`
    //     keeps its initial value of 1.
    //   * Explicit `=YES` → `autobeaconlist = 1`.
    //   * Anything else (`=NO`, `=0`, junk) → `autobeaconlist = 0`.
    //   * Auto-discovery runs ONLY when `autobeaconlist == 1`.
    //     Setting AUTO=NO with an empty `EPICS_CAS_BEACON_ADDR_LIST`
    //     yields an empty beacon list (C prints
    //     "Warning: RSRV has empty beacon address list").
    //
    // The previous Rust gate was `AUTO==YES || beacon_addrs.is_empty()`,
    // which re-enabled discovery whenever the operator hadn't
    // listed any explicit beacon destinations — even with AUTO=NO.
    // That overrode the site's deliberate "no broadcast" intent
    // (e.g. when only multicast targets are wanted via interface
    // setup, or when running fully isolated). Honour AUTO==NO
    // strictly; only run discovery when AUTO is YES.
    let auto_beacon = epics_base_rs::runtime::env::get("EPICS_CAS_AUTO_BEACON_ADDR_LIST");
    let auto_on = match auto_beacon.as_deref() {
        // Unset or empty → C `envGetBoolConfigParam` returns -1,
        // initial `autobeaconlist = 1` survives.
        None | Some("") => true,
        Some(s) => s.eq_ignore_ascii_case("YES"),
    };
    // mixed-0.0.0.0+specific check runs UNCONDITIONALLY, not
    // just under `if auto_on`. C `rsrv/caservertask.c:390-392`
    // `cantProceed`s on this combination regardless of
    // `EPICS_CAS_AUTO_BEACON_ADDR_LIST` (the per-iteration
    // `if(!doautobeacon) continue` at line 374-375 only short-circuits
    // the auto-population loop body; the cantProceed sits AFTER the
    // loop). Pre-fix Rust nested the warn inside `if auto_on`, so the
    // misconfig escaped detection entirely when AUTO=NO; the IOC
    // booted with conflicting wildcard + specific binds and either
    // one of the binds failed silently or both succeeded with
    // undefined kernel routing behaviour.
    let intf_specific: Vec<Ipv4Addr> = cfg
        .intf_addrs
        .iter()
        .copied()
        .filter(|ip| !ip.is_unspecified())
        .collect();
    let intf_has_wildcard = cfg.intf_addrs.iter().any(|ip| ip.is_unspecified());
    if !intf_specific.is_empty() && intf_has_wildcard {
        return Err(CaError::Protocol(
            "EPICS_CAS_INTF_ADDR_LIST may not mix 0.0.0.0 with specific interface IPs \
             (rsrv `cantProceed` parity, caservertask.c:390-392). \
             Use either 0.0.0.0 alone or a list of specific interface IPs."
                .to_string(),
        ));
    }
    if auto_on {
        // C `rsrv/caservertask.c:374-388` filters auto-beacon
        // expansion by `casIntfAddrList` when specific (non-wildcard)
        // interface IPs are listed — beacons only go out via those
        // NICs' broadcasts. Pre-fix Rust unconditionally walked every
        // non-loopback interface via `discover_broadcast_addrs()`,
        // leaking IOC presence onto unrelated networks. If
        // `cfg.intf_addrs` lists specific IPs (not just `0.0.0.0`),
        // derive beacon broadcasts only from those interfaces.
        let bcast_iter: Vec<Ipv4Addr> = if !intf_specific.is_empty() {
            // Restrict to broadcasts of the listed interfaces.
            intf_specific
                .iter()
                .filter_map(|ip| broadcast_for_ip(*ip))
                .collect()
        } else {
            discover_broadcast_addrs()
        };
        for bcast in bcast_iter {
            let entry = SocketAddr::V4(SocketAddrV4::new(bcast, beacon_port));
            if !beacon_addrs.contains(&entry) {
                beacon_addrs.push(entry);
            }
        }
        if beacon_addrs.is_empty() {
            // Last-resort fallback: limited broadcast.  C `rsrv_init`
            // does not add this — it warns and leaves the list empty
            // — but we keep the limited-broadcast fallback for the
            // common single-NIC dev/test case where `getifaddrs`
            // discovery may return no usable bcast. With AUTO=YES this
            // matches the operator's intent.
            beacon_addrs.push(SocketAddr::V4(SocketAddrV4::new(
                Ipv4Addr::BROADCAST,
                beacon_port,
            )));
        }
    } else if beacon_addrs.is_empty() {
        // C prints a warning to stderr.  Surface the same diagnostic
        // so a misconfigured operator sees why no beacons go out.
        eprintln!("Warning: RSRV has empty beacon address list");
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
    //
    // C parity for invalid values (`online_notify.c:58-64`):
    //   if (longStatus || maxPeriod <= 0.0) { maxPeriod = 15.0; }
    // i.e. parse failure OR `<= 0` falls back to the default 15s
    // (not to a synthetic 0.1s floor). A previous Rust revision
    // clamped via `period.max(0.1)`, which silently coerced both
    // 0/negative and tiny-positive values to 100ms. That diverges
    // in two directions: an explicit `0` no longer behaves like
    // "use default", and a deliberately tiny positive (e.g. 0.05
    // in a soak test) gets raised against the operator's wishes.
    // Match C: accept any strictly-positive parsed value as-is;
    // fall back to default for parse-failure or non-positive.
    let raw_period = epics_base_rs::runtime::env::get("EPICS_CAS_BEACON_PERIOD")
        .or_else(|| epics_base_rs::runtime::env::get("EPICS_CA_BEACON_PERIOD"));
    if let Some(period) = raw_period.and_then(|s| s.parse::<f64>().ok()) {
        if period > 0.0 && period.is_finite() {
            cfg.beacon_period = Duration::from_secs_f64(period);
        }
        // else: keep default (15s) — matches C's `maxPeriod <= 0.0` branch.
    }

    Ok(cfg)
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

/// C `osiLocalAddr` (libcom `osi/osdNetIfAddrs.c:167-215`, `osiLocalAddrOnce`):
/// the address of the FIRST interface that is up (`IFF_UP`), `AF_INET`, and
/// NOT loopback (`IFF_LOOPBACK`). When only loopback exists — or interface
/// enumeration fails — C falls back to `INADDR_LOOPBACK`, and so do we.
///
/// C computes this once per process behind `epicsThreadOnce` and hands out the
/// cached result; the `OnceLock` mirrors that (an interface list that changes
/// under a running client is not re-read by C either).
///
/// Caveat, shared with the sibling [`discover_broadcast_addrs`]: `if_addrs`
/// does not expose `IFF_UP`, so a down interface that still carries an address
/// would be picked here where C skips it.
pub fn osi_local_addr() -> Ipv4Addr {
    static CACHED: std::sync::OnceLock<Ipv4Addr> = std::sync::OnceLock::new();
    *CACHED.get_or_init(|| {
        let Ok(ifs) = if_addrs::get_if_addrs() else {
            return Ipv4Addr::LOCALHOST;
        };
        ifs.into_iter()
            .find_map(|iface| match (iface.is_loopback(), iface.ip()) {
                (false, IpAddr::V4(v4)) => Some(v4),
                _ => None,
            })
            .unwrap_or(Ipv4Addr::LOCALHOST)
    })
}

/// Return the IPv4 broadcast address of the up, non-loopback interface
/// whose primary IP equals `match_ip`. Mirrors the
/// `pMatchAddr->ia.sin_addr.s_addr != htonl(INADDR_ANY)` branch in libcom
/// `osdNetIfAddrs.c::osiSockDiscoverBroadcastAddresses` — the C IOC
/// builds a list of broadcast addrs filtered to the matching interface
/// before binding the secondary `udpbcast` socket in
/// `caservertask.c::start_tcp_server_tasks` (lines 670-708).
///
/// Returns `None` when:
///   * `match_ip` is the unspecified address (caller should never call
///     in that case — the primary 0.0.0.0 socket already gets broadcasts);
///   * `match_ip` is loopback (C special-cases this to "loopback as
///     broadcast", but a loopback responder never needs a second
///     broadcast bind);
///   * no matching interface was found, or that interface lacks a
///     broadcast addr (point-to-point links / odd kernel configs);
///   * the discovered broadcast is `0.0.0.0` (libcom drops these).
pub fn broadcast_for_ip(match_ip: Ipv4Addr) -> Option<Ipv4Addr> {
    if match_ip.is_unspecified() || match_ip.is_loopback() {
        return None;
    }
    let ifs = if_addrs::get_if_addrs().ok()?;
    for iface in ifs {
        if iface.is_loopback() {
            continue;
        }
        let if_addrs::IfAddr::V4(v4) = iface.addr else {
            continue;
        };
        if v4.ip != match_ip {
            continue;
        }
        if let Some(b) = v4.broadcast {
            if !b.is_unspecified() {
                return Some(b);
            }
        }
        // `if_addrs` only fills `broadcast` for `IFF_BROADCAST`
        // interfaces. For `IFF_POINTOPOINT` (VPN tun, PPP, WireGuard)
        // C `osdNetIfAddrs.c:130-151` substitutes `ifa_dstaddr` —
        // beacons go to the remote tunnel endpoint. Fall through to a
        // direct `getifaddrs` walk that reads dstaddr for the
        // matched interface.
        #[cfg(unix)]
        {
            if let Some(dst) = ifa_dstaddr_for_ipv4(match_ip) {
                return Some(dst);
            }
        }
        return None;
    }
    None
}

/// walk `getifaddrs(3)` directly to extract `ifa_dstaddr`
/// for the interface whose `ifa_addr` matches `match_ip` AND
/// carries `IFF_POINTOPOINT`. The `if_addrs` crate only exposes
/// `broadcast` for `IFF_BROADCAST` interfaces; P2P interfaces
/// (VPN tun, PPP, WireGuard) need this path or beacons toward the
/// tunnel peer are silently dropped from auto-expansion.
/// Mirrors C `osdNetIfAddrs.c:130-151`.
#[cfg(unix)]
fn ifa_dstaddr_for_ipv4(match_ip: Ipv4Addr) -> Option<Ipv4Addr> {
    // SAFETY: `getifaddrs` returns a linked list of `ifaddrs`
    // structs we walk read-only and free via `freeifaddrs` before
    // returning. All pointer reads are guarded against null and
    // the matched ipv4 octets are copied out as `[u8; 4]` before
    // the free.
    unsafe {
        let mut head: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut head) != 0 || head.is_null() {
            return None;
        }
        let mut result: Option<Ipv4Addr> = None;
        let mut cur = head;
        while !cur.is_null() {
            let entry = &*cur;
            let next = entry.ifa_next;
            // Must be IPv4 AF_INET with the matched ip + POINTOPOINT
            // flag. `ifa_addr` may be null on some interfaces (no
            // address assigned); skip those.
            if !entry.ifa_addr.is_null()
                && (*entry.ifa_addr).sa_family as i32 == libc::AF_INET
                && entry.ifa_flags as libc::c_int & libc::IFF_POINTOPOINT != 0
            {
                let in4: &libc::sockaddr_in = &*(entry.ifa_addr as *const libc::sockaddr_in);
                let ip_octets = u32::from_be(in4.sin_addr.s_addr).to_be_bytes();
                let if_ip = Ipv4Addr::from(ip_octets);
                // `ifa_dstaddr` on macOS/BSD; on Linux the `ifaddrs`
                // struct carries the point-to-point destination in
                // the `ifa_ifu` union field — `libc` exposes it by
                // that name. Both are `*mut sockaddr`.
                #[cfg(target_os = "linux")]
                let dstaddr = entry.ifa_ifu;
                #[cfg(not(target_os = "linux"))]
                let dstaddr = entry.ifa_dstaddr;
                if if_ip == match_ip && !dstaddr.is_null() {
                    let dst4: &libc::sockaddr_in = &*(dstaddr as *const libc::sockaddr_in);
                    let dst_octets = u32::from_be(dst4.sin_addr.s_addr).to_be_bytes();
                    let dst_ip = Ipv4Addr::from(dst_octets);
                    if !dst_ip.is_unspecified() {
                        result = Some(dst_ip);
                        break;
                    }
                }
            }
            cur = next;
        }
        libc::freeifaddrs(head);
        result
    }
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
    fn broadcast_for_ip_rejects_unspecified_and_loopback() {
        // C `osdNetIfAddrs.c:42-54` special-cases loopback (returns
        // INADDR_LOOPBACK as the "broadcast") but `caservertask.c:677`
        // gates the secondary-bind path on `!= INADDR_ANY`. We collapse
        // both cases by returning None for unspecified and loopback
        // inputs — the caller (`udp.rs::run_single_responder`) only
        // ever opens a second responder when the result is `Some`.
        assert_eq!(broadcast_for_ip(Ipv4Addr::UNSPECIFIED), None);
        assert_eq!(broadcast_for_ip(Ipv4Addr::LOCALHOST), None);
    }

    #[test]
    fn broadcast_for_ip_unknown_address_returns_none() {
        // 198.51.100.0/24 is RFC 5737 documentation space — no host
        // machine will have it on a real interface. Lookup must
        // gracefully return None rather than fabricating a broadcast.
        assert_eq!(broadcast_for_ip(Ipv4Addr::new(198, 51, 100, 1)), None);
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

        let cfg = from_env().expect("from_env in test");
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

        let cfg = from_env().expect("from_env in test");
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

    /// C `online_notify.c:58-64` parity for `EPICS_CAS_BEACON_PERIOD`:
    /// any value `<= 0.0` (and parse-failure) falls back to the 15s
    /// default — there is no 100ms floor. Verify that explicit 0,
    /// negatives, and parse failures all keep the default; tiny
    /// positives are accepted verbatim.
    #[test]
    #[serial_test::serial]
    fn from_env_beacon_period_matches_c_default_on_nonpositive() {
        let saved = std::env::var("EPICS_CAS_BEACON_PERIOD").ok();
        let saved_legacy = std::env::var("EPICS_CA_BEACON_PERIOD").ok();
        let saved_auto = std::env::var("EPICS_CAS_AUTO_BEACON_ADDR_LIST").ok();
        // SAFETY: serial_test::serial.
        unsafe {
            std::env::remove_var("EPICS_CA_BEACON_PERIOD");
            std::env::set_var("EPICS_CAS_AUTO_BEACON_ADDR_LIST", "NO");
            std::env::set_var("EPICS_CAS_BEACON_PERIOD", "0");
        }
        let cfg = from_env().expect("from_env in test");
        assert_eq!(
            cfg.beacon_period,
            Duration::from_secs(15),
            "explicit 0 must fall back to 15s default (C parity)"
        );

        // SAFETY: serial_test::serial.
        unsafe {
            std::env::set_var("EPICS_CAS_BEACON_PERIOD", "-5");
        }
        let cfg = from_env().expect("from_env in test");
        assert_eq!(
            cfg.beacon_period,
            Duration::from_secs(15),
            "negative must fall back to 15s default (C parity)"
        );

        // SAFETY: serial_test::serial.
        unsafe {
            std::env::set_var("EPICS_CAS_BEACON_PERIOD", "garbage");
        }
        let cfg = from_env().expect("from_env in test");
        assert_eq!(
            cfg.beacon_period,
            Duration::from_secs(15),
            "parse failure must keep default"
        );

        // Tiny positive: accepted verbatim (no 0.1 floor).
        // SAFETY: serial_test::serial.
        unsafe {
            std::env::set_var("EPICS_CAS_BEACON_PERIOD", "0.05");
        }
        let cfg = from_env().expect("from_env in test");
        assert_eq!(
            cfg.beacon_period,
            Duration::from_secs_f64(0.05),
            "tiny positive must be honoured verbatim — no synthetic floor"
        );

        // SAFETY: serial_test::serial.
        unsafe {
            match saved {
                Some(v) => std::env::set_var("EPICS_CAS_BEACON_PERIOD", v),
                None => std::env::remove_var("EPICS_CAS_BEACON_PERIOD"),
            }
            match saved_legacy {
                Some(v) => std::env::set_var("EPICS_CA_BEACON_PERIOD", v),
                None => std::env::remove_var("EPICS_CA_BEACON_PERIOD"),
            }
            match saved_auto {
                Some(v) => std::env::set_var("EPICS_CAS_AUTO_BEACON_ADDR_LIST", v),
                None => std::env::remove_var("EPICS_CAS_AUTO_BEACON_ADDR_LIST"),
            }
        }
    }

    /// C `caservertask.c:281-287, 415-427` parity for AUTO_BEACON=NO:
    /// when the operator sets `EPICS_CAS_AUTO_BEACON_ADDR_LIST=NO` and
    /// leaves `EPICS_CAS_BEACON_ADDR_LIST` empty, the resulting beacon
    /// list MUST be empty (C prints a warning). A previous Rust
    /// revision auto-populated broadcast addrs whenever the explicit
    /// list was empty regardless of AUTO=NO — re-enabling broadcasts
    /// the operator intentionally disabled.
    #[test]
    #[serial_test::serial]
    fn from_env_auto_beacon_no_with_empty_list_yields_empty() {
        let saved_beacon = std::env::var("EPICS_CAS_BEACON_ADDR_LIST").ok();
        let saved_auto = std::env::var("EPICS_CAS_AUTO_BEACON_ADDR_LIST").ok();
        // SAFETY: serial_test::serial.
        unsafe {
            std::env::remove_var("EPICS_CAS_BEACON_ADDR_LIST");
            std::env::set_var("EPICS_CAS_AUTO_BEACON_ADDR_LIST", "NO");
        }

        let cfg = from_env().expect("from_env in test");
        assert!(
            cfg.beacon_addrs.is_empty(),
            "AUTO=NO with empty explicit list must yield empty beacon_addrs (C parity), got {:?}",
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
