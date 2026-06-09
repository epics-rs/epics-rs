//! Helpers shared across the PVA command-line binaries (`pvget`,
//! `pvput`, `pvmonitor`, `pvinfo`, `pvcall`, `pvlist`, `pvxvct`,
//! `mshim`): timeout parsing, the `-v` effective-config diagnostic,
//! and the `-V` version-information text.

use std::time::Duration;

/// Default PVA CLI timeout in seconds when a user-supplied `-w`
/// is missing or non-finite. Matches `pvget-rs` default of 5.0 s
/// (epics-base `pvget` likewise defaults to 5 s, vs CA tools' 1 s).
pub const DEFAULT_CLI_TIMEOUT_SECS: f64 = 5.0;

/// Convert a user-supplied timeout (CLI `-w`) into a
/// `std::time::Duration`. `Duration::from_secs_f64` panics on NaN /
/// infinity / negative values; clap parses those literally as f64
/// so this guard clamps to [`DEFAULT_CLI_TIMEOUT_SECS`] for any
/// non-finite-or-non-positive input. Mirrors the
/// `epics_ca_rs::cli::timeout_duration` analog (epics-base 1655d68e
/// — defensive handling of pathological floats in tool timeouts).
pub fn timeout_duration(secs: f64) -> Duration {
    let s = if secs.is_finite() && secs > 0.0 {
        secs
    } else {
        DEFAULT_CLI_TIMEOUT_SECS
    };
    Duration::from_secs_f64(s)
}

/// CLI `-w` timeout policy for tools whose pvxs semantics treat a
/// non-positive `-w` as **no deadline** rather than the 5 s clamp of
/// [`timeout_duration`].
///
/// pvxs `-w 0` does not mean one thing across the tools, so a single
/// duration-valued helper cannot serve all of them. `pvxlist` documents
/// `-w 0` as "disables timeout" and waits with `done.wait()` (no
/// deadline) in BOTH discovery and query mode
/// (`tools/list.cpp:55-58,154-203`). This type names that regime so a
/// tool derives one consistent policy from `-w` and applies it
/// identically in every mode, instead of one mode treating `-w 0` as
/// no-deadline while another clamps it to 5 s.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimeoutPolicy {
    /// A bounded operation deadline.
    Finite(Duration),
    /// No deadline — wait until the operation completes or the user
    /// interrupts.
    Forever,
}

impl TimeoutPolicy {
    /// `-w 0` ("no timeout") cannot be expressed as a truly unbounded
    /// wait on the Duration-based client builder
    /// ([`crate::client::PvaClient`]'s `timeout` takes a `Duration`, fed
    /// to `tokio::time::timeout`), so [`TimeoutPolicy::Forever`] is
    /// encoded as a ~10-year sentinel the operation timeout cannot
    /// realistically reach — operationally a no-deadline wait for an
    /// interactive CLI, and far below any `Instant` overflow. Matches
    /// pvxs `pvxlist`'s `done.wait()` (`tools/list.cpp:154-203`).
    const FOREVER_SENTINEL: Duration = Duration::from_secs(10 * 365 * 24 * 3600);

    /// `pvxlist`'s `-w` rule: a finite, strictly-positive value is a
    /// bounded deadline; `0`, a negative, or a non-finite value
    /// (NaN/±Inf) is "no timeout" (`tools/list.cpp:55-58,154-203`). This
    /// is the no-timeout-on-zero regime — the inverse of `pvxcall`'s
    /// immediate-on-zero and distinct from the 5 s clamp
    /// ([`timeout_duration`]).
    pub fn wait_or_forever(secs: f64) -> Self {
        if secs.is_finite() && secs > 0.0 {
            TimeoutPolicy::Finite(Duration::from_secs_f64(secs))
        } else {
            TimeoutPolicy::Forever
        }
    }

    /// The bounded wait duration, or `None` for [`TimeoutPolicy::Forever`].
    /// A discovery loop maps this to an `Option<Instant>` deadline so
    /// `Forever` becomes a genuinely unbounded receive wait.
    pub fn finite_duration(self) -> Option<Duration> {
        match self {
            TimeoutPolicy::Finite(d) => Some(d),
            TimeoutPolicy::Forever => None,
        }
    }

    /// The operation timeout to hand the Duration-based client builder.
    /// `Forever` maps to [`Self::FOREVER_SENTINEL`].
    pub fn op_timeout(self) -> Duration {
        match self {
            TimeoutPolicy::Finite(d) => d,
            TimeoutPolicy::Forever => Self::FOREVER_SENTINEL,
        }
    }
}

/// The `done.wait(timeout)` `-w` rule shared by every pvxs tool that
/// waits on an `epicsEvent` for operation completion (`pvxget`, `pvxput`,
/// `pvxinfo`, `pvxcall`) — distinct from both [`timeout_duration`] (the
/// 5 s clamp) and [`TimeoutPolicy::wait_or_forever`] (no-timeout-on-zero).
///
/// Those tools parse `-w` into a `double timeout` and, after issuing the
/// operation, wait with `done.wait(timeout)` (`tools/get.cpp:72,132`,
/// `tools/put.cpp:64,153`, `tools/info.cpp:66,112`,
/// `tools/call.cpp:44-65,125-154`). EPICS `epicsEvent::wait(double)`
/// treats a timeout of zero or less as `tryWait()` — a non-blocking poll
/// that returns immediately (`libcom/src/osi/epicsEvent.h:101-107,
/// 192-201`). So `-w 0` on any of them is an immediate completion poll,
/// neither a 5 s wait (the prior `timeout_duration` behavior) nor
/// pvxlist's no-deadline wait.
///
/// Maps a finite, strictly-positive `-w` to that bounded duration and
/// any non-positive / non-finite value to `Duration::ZERO`, which the
/// client operation path (`tokio::time::timeout`) treats as an immediate
/// timeout — the `tryWait()` analogue.
pub fn wait_timeout_duration(secs: f64) -> Duration {
    if secs.is_finite() && secs > 0.0 {
        Duration::from_secs_f64(secs)
    } else {
        Duration::ZERO
    }
}

/// The shared `-V` / `--version` text for the PVA CLI tools, the
/// analogue of pvxs `version_information` (`src/describe.cpp:135-140`).
///
/// pvxs routes every tool's `-V` through
/// `std::cout << pvxs::version_information; return 0;`
/// (`tools/get.cpp:62-64`, `put.cpp:55`, `list.cpp:75-82`,
/// `monitor.cpp:63`, `call.cpp:55`, …), which prints the PVXS library
/// version, the EPICS Base version string (`EPICS_VERSION_STRING`,
/// `src/describe.cpp:138`), and the libevent version — the
/// protocol/toolchain stack an operator pastes into a bug report.
/// Clap's generated `--version` reports only `<binary> <crate-version>`,
/// losing that dependency context.
///
/// The Rust analogue reports the three facts the linked libraries know
/// at compile time without a build script:
///   - the `epics-pva-rs` crate version (the PVA implementation, the
///     PVXS analogue),
///   - the EPICS Base release the port targets
///     ([`epics_base_rs::EPICS_BASE_VERSION`], from
///     `configure/CONFIG_BASE_VERSION`) — the `EPICS_VERSION_STRING`
///     analogue, and
///   - the PVA wire-protocol version it speaks
///     ([`crate::proto::PVA_VERSION`]) — the interop-critical fact a
///     "talks to pvxs/Java but not X" report needs, the analogue of
///     pvxs's libevent/runtime line.
///
/// The EPICS Base version names the upstream C release being ported,
/// which tracks EPICS Base's release cadence rather than the
/// `epics-base-rs` crate version, so it is a genuinely separate line
/// and not a duplicate of [`crate::VERSION`].
pub fn version_information() -> &'static str {
    use std::sync::OnceLock;
    static V: OnceLock<String> = OnceLock::new();
    V.get_or_init(|| {
        format!(
            "epics-pva-rs {}\nEPICS Base {}\nPVA protocol version {}\n",
            crate::VERSION,
            epics_base_rs::EPICS_BASE_VERSION,
            crate::proto::PVA_VERSION
        )
    })
}

/// Print the effective PVA client configuration, the shared `-v`
/// ("make more noise") verbose path for the pvxs-compatible tools.
///
/// pvxs routes `-v` through `verbose=true` and, before issuing any
/// operation, prints `Effective config\n` followed by the client
/// context configuration — `tools/get.cpp:99-100`,
/// `tools/monitor.cpp:97-98`, `tools/put.cpp:109-110`,
/// `tools/call.cpp:122-123`, and `tools/info.cpp:76-79`. Every one of
/// those tools emits the same block, so it lives here as a single owner
/// rather than being re-implemented (or, worse, overloaded onto the
/// output formatter) per binary. The values are read through the
/// resolved [`crate::config`] getters so the output reflects
/// environment + defaults exactly as the client will use them.
pub fn print_effective_config() {
    print!("{}", effective_config_string());
}

/// Render the `Effective config` block as a string (see
/// [`print_effective_config`]). Split out so the verbose output is
/// unit-testable without capturing process stdout.
pub fn effective_config_string() -> String {
    use crate::config;
    use std::fmt::Write;
    let addr_list = std::env::var("EPICS_PVA_ADDR_LIST").unwrap_or_default();
    let name_servers = config::name_servers()
        .iter()
        .map(|a| a.to_string())
        .collect::<Vec<_>>()
        .join(" ");
    let interfaces = config::list_intf_addresses()
        .iter()
        .map(|a| a.to_string())
        .collect::<Vec<_>>()
        .join(" ");
    let mut s = String::new();
    let _ = writeln!(s, "Effective config");
    let _ = writeln!(s, "  EPICS_PVA_ADDR_LIST={addr_list}");
    let _ = writeln!(
        s,
        "  EPICS_PVA_AUTO_ADDR_LIST={}",
        if config::auto_addr_list_enabled() {
            "YES"
        } else {
            "NO"
        }
    );
    let _ = writeln!(s, "  EPICS_PVA_SERVER_PORT={}", config::server_port());
    let _ = writeln!(s, "  EPICS_PVA_BROADCAST_PORT={}", config::broadcast_port());
    let _ = writeln!(s, "  EPICS_PVA_NAME_SERVERS={name_servers}");
    let _ = writeln!(s, "  interfaces={interfaces}");
    s
}

/// Resolve a PVA tool endpoint **body** (the address part of `mshim
/// -L/-F` and `pvxvct -B`) to a single IPv4 address. A literal IPv4
/// string is used directly; any other token is resolved through the
/// system resolver and the first IPv4 result is taken.
///
/// pvxs routes these endpoints through `SockEndpoint` →
/// `SockAddr::setAddress` (`util.cpp:523-538`), which accepts DNS
/// hostnames and prefers IPv4 "for maximum compatibility". Both tools
/// are IPv4-only (mshim is an IPv4 multicast shim whose `parseEP`
/// rejects any non-AF_INET resolved endpoint, `mshim.cpp:66-68`; pvxvct
/// binds AF_INET), so a name with no IPv4 address is an error rather
/// than an IPv6 fallback. This is the single owner both tools share, so
/// they cannot diverge on hostname handling the way they did when one
/// resolved names and the other required a literal `IpAddr`.
pub fn resolve_host_ipv4(host: &str) -> Result<std::net::Ipv4Addr, String> {
    use std::net::{IpAddr, Ipv4Addr, ToSocketAddrs};
    if let Ok(v4) = host.parse::<Ipv4Addr>() {
        return Ok(v4);
    }
    (host, 0u16)
        .to_socket_addrs()
        .map_err(|e| format!("cannot resolve {host:?}: {e}"))?
        .find_map(|sa| match sa.ip() {
            IpAddr::V4(v4) => Some(v4),
            IpAddr::V6(_) => None,
        })
        .ok_or_else(|| format!("no IPv4 address for {host:?}"))
}

/// Resolve an endpoint `@iface` suffix to the interface's IPv4 address.
/// Accepts either a literal IPv4 address (returned verbatim) or an OS
/// interface name (`eth0`, `en0`, `lo0`), looked up via `getifaddrs`.
///
/// pvxs normalizes the multicast `@iface` of a `SockEndpoint` through
/// `IfaceMap`, which accepts both an interface name and an interface
/// IPv4 address (`evhelper.cpp:556-575`). Resolving the suffix only as a
/// DNS host — as the cable tester previously did — made
/// `pvxvct -B 224.0.1.1@en0` fail unless `en0` happened to exist in DNS.
/// This is the single owner both tools share for `@iface` resolution.
pub fn resolve_iface_ipv4(spec: &str) -> Result<std::net::Ipv4Addr, String> {
    if let Ok(v4) = spec.parse::<std::net::Ipv4Addr>() {
        return Ok(v4);
    }
    #[cfg(unix)]
    {
        iface_name_to_ipv4(spec)
    }
    #[cfg(not(unix))]
    {
        Err(format!(
            "interface-name override {spec:?} requires a Unix host; \
             pass the interface's IPv4 address instead"
        ))
    }
}

/// Look up an interface's first IPv4 address by name via `getifaddrs`.
#[cfg(unix)]
fn iface_name_to_ipv4(name: &str) -> Result<std::net::Ipv4Addr, String> {
    use std::ffi::CStr;
    use std::net::Ipv4Addr;

    // SAFETY: getifaddrs allocates a linked list we free via
    // freeifaddrs; every pointer is null-checked before deref.
    unsafe {
        let mut ifap: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut ifap) != 0 {
            return Err(format!(
                "getifaddrs failed: {}",
                std::io::Error::last_os_error()
            ));
        }
        let mut cur = ifap;
        let mut found: Option<Ipv4Addr> = None;
        while !cur.is_null() {
            let ifa = &*cur;
            if !ifa.ifa_name.is_null() && !ifa.ifa_addr.is_null() {
                let ifa_name = CStr::from_ptr(ifa.ifa_name).to_string_lossy();
                let sa = &*ifa.ifa_addr;
                if ifa_name == name && sa.sa_family as i32 == libc::AF_INET {
                    let sin = &*(ifa.ifa_addr as *const libc::sockaddr_in);
                    // s_addr is in network byte order.
                    let addr = Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr));
                    found = Some(addr);
                    break;
                }
            }
            cur = ifa.ifa_next;
        }
        libc::freeifaddrs(ifap);
        found.ok_or_else(|| format!("interface {name:?} has no IPv4 address"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeout_duration_clamps_pathological_floats() {
        assert_eq!(
            timeout_duration(f64::NAN).as_secs_f64(),
            DEFAULT_CLI_TIMEOUT_SECS
        );
        assert_eq!(
            timeout_duration(f64::INFINITY).as_secs_f64(),
            DEFAULT_CLI_TIMEOUT_SECS
        );
        assert_eq!(
            timeout_duration(f64::NEG_INFINITY).as_secs_f64(),
            DEFAULT_CLI_TIMEOUT_SECS
        );
        assert_eq!(
            timeout_duration(-1.0).as_secs_f64(),
            DEFAULT_CLI_TIMEOUT_SECS
        );
        assert_eq!(
            timeout_duration(0.0).as_secs_f64(),
            DEFAULT_CLI_TIMEOUT_SECS
        );
    }

    #[test]
    fn timeout_duration_preserves_positive_finite() {
        let d = timeout_duration(3.5);
        assert!((d.as_secs_f64() - 3.5).abs() < 1e-9);
    }

    /// `pvxlist` `-w 0` means "no timeout": the policy is `Forever`, and
    /// non-positive / non-finite inputs collapse the same way
    /// (`tools/list.cpp:55-58,154-203`). This is the inverse of the
    /// `timeout_duration` 5 s clamp above.
    #[test]
    fn wait_or_forever_treats_nonpositive_as_forever() {
        for secs in [0.0, -1.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert_eq!(
                TimeoutPolicy::wait_or_forever(secs),
                TimeoutPolicy::Forever,
                "secs={secs}"
            );
        }
        // Forever has no finite deadline (discovery → unbounded wait)
        // and a far-future operation timeout (query → effectively no
        // deadline).
        assert_eq!(TimeoutPolicy::Forever.finite_duration(), None);
        assert!(TimeoutPolicy::Forever.op_timeout() >= Duration::from_secs(365 * 24 * 3600));
    }

    /// A finite, strictly-positive `-w` is a bounded deadline, applied
    /// identically by discovery (`finite_duration`) and query
    /// (`op_timeout`).
    #[test]
    fn wait_or_forever_preserves_positive_finite() {
        let p = TimeoutPolicy::wait_or_forever(3.0);
        assert_eq!(p, TimeoutPolicy::Finite(Duration::from_secs(3)));
        assert_eq!(p.finite_duration(), Some(Duration::from_secs(3)));
        assert_eq!(p.op_timeout(), Duration::from_secs(3));
    }

    /// `-w 0` on any `done.wait(timeout)` tool (`pvxget`/`pvxput`/
    /// `pvxinfo`/`pvxcall`) is an immediate completion poll (epicsEvent
    /// `tryWait`, `epicsEvent.h:101-107`): the mapping yields
    /// `Duration::ZERO`, which the client treats as an immediate timeout
    /// — NOT the 5 s clamp and NOT pvxlist's no-deadline wait.
    #[test]
    fn wait_timeout_duration_maps_nonpositive_to_immediate() {
        for secs in [0.0, -1.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert_eq!(wait_timeout_duration(secs), Duration::ZERO, "secs={secs}");
        }
    }

    /// A finite, strictly-positive `-w` is preserved as a bounded
    /// completion timeout.
    #[test]
    fn wait_timeout_duration_preserves_positive_finite() {
        assert_eq!(wait_timeout_duration(2.5), Duration::from_secs_f64(2.5));
    }

    /// The shared `-v` verbose path emits the pvxs `Effective config`
    /// block with the resolved client configuration keys. This is the
    /// text every pvxs-compatible tool (`pvget`/`pvmonitor`/`pvput`/
    /// `pvcall`/`pvinfo`) prints on `-v`; asserting on the fixed keys
    /// keeps the check environment-independent.
    #[test]
    fn effective_config_emits_config_keys() {
        let s = effective_config_string();
        assert!(s.starts_with("Effective config\n"), "got: {s:?}");
        for key in [
            "EPICS_PVA_ADDR_LIST=",
            "EPICS_PVA_AUTO_ADDR_LIST=",
            "EPICS_PVA_SERVER_PORT=",
            "EPICS_PVA_BROADCAST_PORT=",
            "EPICS_PVA_NAME_SERVERS=",
            "interfaces=",
        ] {
            assert!(s.contains(key), "missing {key:?} in:\n{s}");
        }
    }

    /// The shared `-V` text reports more than `<binary> <crate-version>`:
    /// it names the `epics-pva-rs` library version, the EPICS Base
    /// release the port targets, and the PVA wire-protocol version (the
    /// pvxs `version_information` analogue). This is the
    /// dependency/protocol context clap's crate-only `--version` dropped.
    #[test]
    fn version_information_includes_protocol_stack() {
        let v = super::version_information();
        assert!(
            v.contains(&format!("epics-pva-rs {}", crate::VERSION)),
            "got: {v:?}"
        );
        assert!(
            v.contains(&format!("EPICS Base {}", epics_base_rs::EPICS_BASE_VERSION)),
            "got: {v:?}"
        );
        assert!(
            v.contains(&format!(
                "PVA protocol version {}",
                crate::proto::PVA_VERSION
            )),
            "got: {v:?}"
        );
        // Strictly more than the bare crate version: at least three lines
        // (crate version, EPICS Base version, PVA protocol version).
        assert!(
            v.lines().count() >= 3,
            "version output must carry dependency context, got: {v:?}"
        );
    }

    /// `resolve_host_ipv4` accepts a literal IPv4 verbatim and resolves a
    /// hostname to IPv4, preferring IPv4 over any IPv6 the resolver also
    /// returns (`localhost` is typically dual-stack). pvxs `setAddress`
    /// makes the same IPv4-preferring choice (`util.cpp:529-538`).
    #[test]
    fn resolve_host_ipv4_literal_and_hostname() {
        assert_eq!(
            resolve_host_ipv4("192.168.1.5").unwrap(),
            std::net::Ipv4Addr::new(192, 168, 1, 5)
        );
        let lo = resolve_host_ipv4("localhost").expect("localhost resolves");
        assert!(lo.is_loopback(), "expected IPv4 loopback, got {lo}");
    }

    /// `resolve_iface_ipv4` accepts a literal interface IPv4 address
    /// verbatim and, on Unix, resolves an interface *name* to its IPv4
    /// address — the dual form pvxs `IfaceMap` accepts
    /// (`evhelper.cpp:556-575`). The loopback interface is `lo` on Linux
    /// and `lo0` on macOS/BSD; try both.
    #[test]
    fn resolve_iface_ipv4_literal_and_name() {
        assert_eq!(
            resolve_iface_ipv4("10.0.0.2").unwrap(),
            std::net::Ipv4Addr::new(10, 0, 0, 2)
        );
        #[cfg(unix)]
        {
            let lo = resolve_iface_ipv4("lo").or_else(|_| resolve_iface_ipv4("lo0"));
            if let Ok(v4) = lo {
                assert!(v4.is_loopback(), "loopback iface IPv4 expected, got {v4}");
            }
        }
    }
}
