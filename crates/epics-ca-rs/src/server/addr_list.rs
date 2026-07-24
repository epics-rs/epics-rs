//! EPICS_CAS_* address-list parsing and broadcast-interface discovery.
//!
//! Mirrors the behaviour of `addAddrToChannelAccessAddressList` in
//! `epics-base/modules/database/src/ioc/rsrv/caservertask.c`, providing
//! parsed address lists for the IOC's UDP search responder and beacon
//! emitter.

use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
// `IpAddr` is used only by the `if-addrs` interface-enumeration helpers, which
// are host-only (their embedded-target stubs return empty results).
#[cfg(not(epics_embedded_target))]
use std::net::IpAddr;
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

/// C `online_notify.c:59` — the beacon period RSRV falls back to, and the
/// number its diagnostic prints. Same 15 s as [`CasUdpConfig::default`], and it
/// comes from the one place that number is declared: the generated `ENV_PARAM`
/// table's `EPICS_CA_BEACON_PERIOD` default (`configure/CONFIG_ENV`).
fn default_beacon_period_secs() -> f64 {
    epics_base_rs::runtime::env_table::EPICS_CA_BEACON_PERIOD
        .default_str()
        .parse()
        .expect("EPICS_CA_BEACON_PERIOD's compiled default is a number")
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
            beacon_period: crate::estdlib::duration_from_secs(default_beacon_period_secs()),
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
///
/// Resolved ONCE per process. C reads this configuration once — `rsrv_init`
/// builds the address lists, and the single beacon thread
/// (`online_notify.c:52-64`) reads `EPICS_CAS_BEACON_PERIOD` on its way up —
/// so each diagnostic below is printed exactly once by a C IOC. The port calls
/// this from three points in server startup (`bind_tcp_listeners`,
/// `bind_sockets`, `CaServer::run`), which tripled every one of them: the
/// compiled `softIoc` prints "float fetch failed" once for a bad beacon period,
/// this printed it three times. Memoizing the whole resolution — not just the
/// beacon period — also stops the interface discovery and the empty-list
/// warning from repeating, and is what `EPICS_CA_*` client-side resolution
/// already does.
pub fn from_env() -> CaResult<CasUdpConfig> {
    static RESOLVED: std::sync::OnceLock<Result<CasUdpConfig, String>> = std::sync::OnceLock::new();
    // `resolve_from_env`'s only failure is `CaError::Protocol`, so caching the
    // message and rebuilding the error loses nothing. `CaError` is not `Clone`.
    RESOLVED
        .get_or_init(|| resolve_from_env().map_err(|e| e.to_string()))
        .clone()
        .map_err(CaError::Protocol)
}

/// The uncached resolution behind [`from_env`] — every env read, every
/// diagnostic, every interface probe. Tests drive this directly; production
/// code goes through the memoized [`from_env`].
fn resolve_from_env() -> CaResult<CasUdpConfig> {
    let mut cfg = CasUdpConfig::default();

    if let Some(list) = epics_base_rs::runtime::env_table::EPICS_CAS_INTF_ADDR_LIST.get() {
        // C `caservertask.c:341-343` tokenizes with the server's UDP port —
        // which is the port its duplicate warning dots the address with.
        let udp_port = epics_base_rs::runtime::net::cas_server_port();
        let parsed = parse_ipv4_list(&list, "EPICS_CAS_INTF_ADDR_LIST", udp_port);
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

    // Server-side beacon port: EPICS_CAS_BEACON_PORT when configured,
    // else EPICS_CA_REPEATER_PORT, each resolved by the one owner of
    // C `envGetInetPortConfigParam` (rsrv/caservertask.c:501-508).
    let beacon_port = epics_base_rs::runtime::net::cas_beacon_port();

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
    if let Some(list) = epics_base_rs::runtime::env_table::EPICS_CAS_BEACON_ADDR_LIST.get() {
        beacon_addrs.extend(parse_addr_list(
            &list,
            "EPICS_CAS_BEACON_ADDR_LIST",
            beacon_port,
        ));
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
    let auto_beacon = epics_base_rs::runtime::env_table::EPICS_CAS_AUTO_BEACON_ADDR_LIST.get();
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
        // No `contains` guard: on the server C dedups the WHOLE beacon list —
        // user entries and auto-discovered broadcasts together — with one
        // `removeDuplicateAddresses(&beaconAddrList, &temp, 0)` at
        // `caservertask.c:438`, which reports every repeat it drops. Two NICs on
        // one subnet, or an operator who lists a broadcast the discovery already
        // found, are exactly the cases C reports and the port swallowed.
        for bcast in bcast_iter {
            beacon_addrs.push(SocketAddr::V4(SocketAddrV4::new(bcast, beacon_port)));
        }
    }
    // C `caservertask.c:438` — outside the `if (autobeaconlist)`, so the user's
    // own list is deduped and reported even when auto-discovery is off.
    let mut beacon_addrs = crate::iocinf::remove_duplicate_addresses(beacon_addrs, |a| *a);
    if auto_on {
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

    if let Some(list) = epics_base_rs::runtime::env_table::EPICS_CAS_IGNORE_ADDR_LIST.get() {
        // C `caservertask.c:450-451` builds this one with `port = 0`, so the
        // duplicate it reports is dotted as `10.1.2.3:0` — captured from the
        // compiled `softIoc`, which is why the port is not the server's here.
        cfg.ignore_addrs = parse_ipv4_list(&list, "EPICS_CAS_IGNORE_ADDR_LIST", 0);
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
    //
    // The fallback is C's `envGetConfigParamPtr` PRESENCE test, not a
    // parse-success test: an invalid `EPICS_CAS_BEACON_PERIOD` does not
    // silently promote the legacy var. Parsing and the `Duration`
    // conversion are `crate::estdlib` (C `epicsScanDouble`), so `inf` is
    // an accepted — never-firing — period rather than a panic, and NaN
    // (which fails `> 0.0`, like every C comparison against it) keeps
    // the default.
    use epics_base_rs::runtime::env_table::{EPICS_CA_BEACON_PERIOD, EPICS_CAS_BEACON_PERIOD};
    let param = if EPICS_CAS_BEACON_PERIOD.get().is_some() {
        EPICS_CAS_BEACON_PERIOD
    } else {
        EPICS_CA_BEACON_PERIOD
    };
    match param.double() {
        Ok(period) if period > 0.0 => {
            cfg.beacon_period = crate::estdlib::duration_from_secs(period);
        }
        // Unresolvable: C reads the compiled "15.0" default of the legacy var,
        // silently. Keep the default period, print nothing.
        Err(epics_base_rs::runtime::env::EnvDoubleError::Unresolvable) => {}
        // Parse failure OR `<= 0.0` — C `online_notify.c:58-64`. C names
        // EPICS_CAS_BEACON_PERIOD in both lines even when the value it
        // fetched came from the deprecated var, so this does too.
        _ => {
            eprintln!("EPICS \"EPICS_CAS_BEACON_PERIOD\" float fetch failed");
            eprintln!(
                "Setting \"EPICS_CAS_BEACON_PERIOD\" = {:.6}",
                default_beacon_period_secs()
            );
        }
    }

    Ok(cfg)
}

/// One `EPICS_CAS_*` address list, tokenized and deduped exactly as C does it:
/// `addAddrToChannelAccessAddressList` then
/// `removeDuplicateAddresses(…, silent=0)` (`caservertask.c:341-343`,
/// `:413-438`, `:450-451`). Both diagnostics — the bad token and the discarded
/// duplicate — belong to those two functions and are printed by them.
pub fn parse_addr_list(list: &str, env_name: &str, default_port: u16) -> Vec<SocketAddr> {
    let tokens =
        crate::iocinf::add_addr_to_channel_access_address_list(list, env_name, default_port);
    crate::iocinf::remove_duplicate_addresses(tokens, |t| t.sock)
        .into_iter()
        .map(|t| t.sock)
        .collect()
}

/// The interface and ignore lists, which C builds through the same two
/// functions and then uses only the IP half of: `casIntfAddrList` is bound per
/// interface, `casIgnoreAddrs` is matched against a datagram's source IP. The
/// port each entry carries still decides what counts as a duplicate and what
/// the duplicate warning prints, which is why the list is deduped BEFORE the
/// port is dropped.
fn parse_ipv4_list(list: &str, env_name: &str, default_port: u16) -> Vec<Ipv4Addr> {
    parse_addr_list(list, env_name, default_port)
        .into_iter()
        .filter_map(|a| match a {
            SocketAddr::V4(v4) => Some(*v4.ip()),
            SocketAddr::V6(_) => None,
        })
        .collect()
}

/// Discover IPv4 broadcast addresses for all up, non-loopback interfaces.
/// Returns an empty vec if interface enumeration fails (e.g. unsupported OS).
///
/// Host-only: interface enumeration (`if-addrs`) does not build for RTEMS or
/// VxWorks. The embedded build (blocking `std::net` driver) emits no
/// beacons/broadcast, so the shared `resolve_from_env` config path takes the
/// empty stub below.
#[cfg(not(epics_embedded_target))]
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

/// Embedded-target stub — `if-addrs` is host-only. The blocking driver
/// emits no beacons/broadcast, so auto-discovery yields nothing.
#[cfg(epics_embedded_target)]
pub fn discover_broadcast_addrs() -> Vec<Ipv4Addr> {
    Vec::new()
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
#[cfg(not(epics_embedded_target))]
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

/// Embedded-target stub — `if-addrs` is host-only. Falls back to
/// `INADDR_LOOPBACK`, the same value C `osiLocalAddr` returns when interface
/// enumeration finds no up, non-loopback `AF_INET` interface.
#[cfg(epics_embedded_target)]
pub fn osi_local_addr() -> Ipv4Addr {
    Ipv4Addr::LOCALHOST
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
#[cfg(not(epics_embedded_target))]
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

/// Embedded-target stub — `if-addrs` is host-only. The blocking driver binds
/// no secondary broadcast responder, so no per-interface broadcast is
/// resolved.
#[cfg(epics_embedded_target)]
pub fn broadcast_for_ip(_match_ip: Ipv4Addr) -> Option<Ipv4Addr> {
    None
}

/// walk `getifaddrs(3)` directly to extract `ifa_dstaddr`
/// for the interface whose `ifa_addr` matches `match_ip` AND
/// carries `IFF_POINTOPOINT`. The `if_addrs` crate only exposes
/// `broadcast` for `IFF_BROADCAST` interfaces; P2P interfaces
/// (VPN tun, PPP, WireGuard) need this path or beacons toward the
/// tunnel peer are silently dropped from auto-expansion.
/// Mirrors C `osdNetIfAddrs.c:130-151`. Host-only: neither the RTEMS nor the
/// VxWorks `libc` has `getifaddrs`/`ifaddrs`, and the embedded-target
/// `broadcast_for_ip` stub never calls this.
#[cfg(all(unix, not(epics_embedded_target)))]
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
        let parsed = parse_addr_list(
            "10.0.0.1 192.168.1.255:5066",
            "EPICS_CAS_BEACON_ADDR_LIST",
            5065,
        );
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].port(), 5065);
        assert_eq!(parsed[1].port(), 5066);
    }

    #[test]
    fn parse_ipv4_list_drops_garbage() {
        let v = parse_ipv4_list("1.2.3.4 not-an-ip 5.6.7.8", "EPICS_CAS_INTF_ADDR_LIST", 0);
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
        assert!(parse_addr_list("", "EPICS_CAS_BEACON_ADDR_LIST", 5065).is_empty());
        assert!(parse_ipv4_list("   ", "EPICS_CAS_INTF_ADDR_LIST", 0).is_empty());
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

        let cfg = resolve_from_env().expect("resolve_from_env in test");
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

        let cfg = resolve_from_env().expect("resolve_from_env in test");
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
        let cfg = resolve_from_env().expect("resolve_from_env in test");
        assert_eq!(
            cfg.beacon_period,
            Duration::from_secs(15),
            "explicit 0 must fall back to 15s default (C parity)"
        );

        // SAFETY: serial_test::serial.
        unsafe {
            std::env::set_var("EPICS_CAS_BEACON_PERIOD", "-5");
        }
        let cfg = resolve_from_env().expect("resolve_from_env in test");
        assert_eq!(
            cfg.beacon_period,
            Duration::from_secs(15),
            "negative must fall back to 15s default (C parity)"
        );

        // SAFETY: serial_test::serial.
        unsafe {
            std::env::set_var("EPICS_CAS_BEACON_PERIOD", "garbage");
        }
        let cfg = resolve_from_env().expect("resolve_from_env in test");
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
        let cfg = resolve_from_env().expect("resolve_from_env in test");
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

        let cfg = resolve_from_env().expect("resolve_from_env in test");
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
