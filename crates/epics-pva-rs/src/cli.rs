//! Helpers shared across the PVA command-line binaries (`pvget`,
//! `pvput`, `pvmonitor`, `pvinfo`, `pvcall`, `pvlist`, `pvxvct`,
//! `mshim`): timeout parsing, the `-v` effective-config diagnostic,
//! and the `-V` version-information text.

use std::time::Duration;

/// Parse the command line, exiting the way the C tools exit.
///
/// clap's own `Error::exit` uses 2 for a usage error. No pvAccess tool
/// does: `getopt`'s three failure arms all `return 1`
/// (`pvget.cpp:340,357,362,365`, `pvput.cpp:342,351,356,359`), and `-h`
/// / `-V` `return 0` (`pvget.cpp:283,300`). Measured on the built
/// binaries: `pvget -Z` exits 1. clap already routes help and version to
/// stdout and errors to stderr, and `use_stderr()` is exactly that
/// split, so only the code is ours.
pub fn parse_or_exit<T: clap::Parser>() -> T {
    parse_or_exit_styled(UsageErrorStyle::PvTools)
}

/// Which upstream family a tool's usage errors follow. Both exit 1;
/// they differ in whether upstream has a message worth reproducing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UsageErrorStyle {
    /// The pvAccess `pvtoolsSrc` family (`pvget`, `pvput`, `pvinfo`,
    /// `pvlist`, `pvcall`, and the monitor mode of `pvget`), whose two
    /// fixed lines `c_option_error` reproduces.
    PvTools,
    /// The pvxs `tools` family (`mshim`, `pvxvct`). Their message is
    /// NOT reproduced: their `getopt` optstring has no leading `:`, so
    /// the `default:` arm runs with `opt == '?'` and
    /// `std::cerr<<char(opt)` prints a literal `?` where the code means
    /// to print the offending letter (`tools/mshim.cpp:271-274`,
    /// `tools/pvxvct.cpp:140-144`). clap's message is kept; only the
    /// exit code is upstream's.
    Pvxs,
}

/// [`parse_or_exit`] for a tool that follows a different upstream
/// family — see [`UsageErrorStyle`].
pub fn parse_or_exit_styled<T: clap::Parser>(style: UsageErrorStyle) -> T {
    let argv: Vec<std::ffi::OsString> = std::env::args_os().collect();
    match T::try_parse_from(&argv) {
        Ok(args) => args,
        Err(e) => {
            let cmd = <T as clap::CommandFactory>::command();
            match (style == UsageErrorStyle::PvTools)
                .then(|| c_option_error(&e, &cmd, &argv))
                .flatten()
            {
                Some(msg) => eprint!("{msg}"),
                None => {
                    let _ = e.print();
                }
            }
            std::process::exit(i32::from(e.use_stderr()));
        }
    }
}

/// C's wording for the two `getopt` failures every pvAccess tool
/// answers itself, or `None` when C has no such answer and clap's own
/// message stands.
///
/// `getopt` is given a leading-`:` optstring (`pvget.cpp:279`), so it
/// reports an unknown letter as `'?'` and a letter whose argument is
/// missing as `':'`, and each arm prints one fixed line naming the
/// letter and the tool (`pvget.cpp:353-362`; identically in
/// `pvput.cpp:349-356`, `pvinfo.cpp:152-159`, `pvlist.cpp:571-578`,
/// `pvcall.cpp:156-163`). clap's text says neither.
///
/// The letter comes from clap's `InvalidArg` context, which names the
/// offending option in its CANONICAL spelling plus a value placeholder
/// — the long form whenever the option has one, whichever form was
/// typed. C only ever has the letter, and every long form here is a
/// port addition on top of a C short option, so a canonical long is
/// mapped back through the command's own argument table. `argv`
/// decides between the two spellings: naming `-F` to someone who typed
/// `--format` would name an option they never used, so a long
/// invocation keeps clap's message. So does an option with no short
/// form at all, and so does any long that `getopt` would never have
/// seen as an option in the first place (it takes `--anything` as the
/// end of options).
fn c_option_error(
    err: &clap::Error,
    cmd: &clap::Command,
    argv: &[std::ffi::OsString],
) -> Option<String> {
    use clap::error::{ContextKind, ContextValue, ErrorKind};

    let ContextValue::String(invalid_arg) = err.get(ContextKind::InvalidArg)? else {
        return None;
    };
    let letter = short_form(invalid_arg.split_whitespace().next()?, cmd, argv)?;
    let exec = cmd.get_name();

    match err.kind() {
        ErrorKind::UnknownArgument => Some(format!(
            "Unrecognized option: '-{letter}'. ('{exec} -h' for help.)\n"
        )),
        // clap folds "no value supplied" into `InvalidValue` with an
        // empty value (`clap_builder/src/error/format.rs:208-221`).
        ErrorKind::InvalidValue if matches!(err.get(ContextKind::InvalidValue), Some(ContextValue::String(v)) if v.is_empty()) => {
            Some(format!(
                "Option '-{letter}' requires an argument. ('{exec} -h' for help.)\n"
            ))
        }
        _ => None,
    }
}

/// The short letter `token` stands for, when the command line used the
/// short spelling. `None` for a long invocation, a long-only option,
/// and anything that is not an option token.
fn short_form(token: &str, cmd: &clap::Command, argv: &[std::ffi::OsString]) -> Option<char> {
    if let Some(long) = token.strip_prefix("--") {
        let short = cmd
            .get_arguments()
            .find(|a| a.get_long() == Some(long))?
            .get_short()?;
        // `--long`, `--long=value`; a prefix abbreviation is clap-only
        // and reads as a long invocation either way.
        let typed_long = argv.iter().filter_map(|a| a.to_str()).any(|a| {
            let Some(spelling) = a.strip_prefix("--") else {
                return false;
            };
            let spelling = spelling.split('=').next().unwrap_or(spelling);
            !spelling.is_empty() && long.starts_with(spelling)
        });
        return (!typed_long).then_some(short);
    }
    let mut letters = token.strip_prefix('-')?.chars();
    let letter = letters.next()?;
    letters.next().is_none().then_some(letter)
}

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
    secs_to_duration(s)
}

/// The single owner of every `-w` second → [`Duration`] conversion in the
/// CLI helpers. `Duration::from_secs_f64` panics above `u64::MAX` seconds,
/// and the `is_finite() && > 0.0` guards each helper applies do not exclude
/// that range — `pvget -w 1e300` passed every guard and then aborted the
/// tool. pvxs's tools hand `-w` to `epicsEvent::wait(double)`, which simply
/// blocks for a duration no run will outlive, so an out-of-range `-w`
/// saturates to [`TimeoutPolicy::FOREVER_SENTINEL`] (~10 years) here rather
/// than panicking. Values the callers have already rejected (NaN, negative)
/// never reach this function.
fn secs_to_duration(secs: f64) -> Duration {
    Duration::try_from_secs_f64(secs).unwrap_or(TimeoutPolicy::FOREVER_SENTINEL)
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
    /// (`client::PvaClient`'s `timeout` takes a `Duration`, fed
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
            TimeoutPolicy::Finite(secs_to_duration(secs))
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
    /// `Forever` maps to `Self::FOREVER_SENTINEL`.
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
/// `tools/call.cpp:44-65,150-155`). EPICS `epicsEvent::wait(double)`
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
        secs_to_duration(secs)
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
/// `tools/call.cpp:122-123`, and `tools/info.cpp:78-79`. Every one of
/// those tools emits the same block, so it lives here as a single owner
/// rather than being re-implemented (or, worse, overloaded onto the
/// output formatter) per binary.
///
/// The configuration shown is the *effective* one. pvxs's
/// `ContextImpl::effective` is the environment config with `expand()`
/// applied (`src/client.cpp:542-547`), and `-v` prints that expanded config
/// (`ctxt.config()`), not the raw environment. So `EPICS_PVA_ADDR_LIST`
/// is the post-`expand()` SEARCH list — the auto-address-list broadcast
/// fan-out folded in and the `AUTO_ADDR_LIST` flag cleared to `NO`
/// (`config.cpp:640-644`) — exactly as the client will search.
#[cfg(not(epics_embedded_target))]
pub fn print_effective_config() {
    print!("{}", effective_config_string());
}

/// Render one resolved [`crate::config::env::Endpoint`] the way pvxs prints
/// an effective address-list entry. `expand()` gives every entry the
/// effective UDP port, so the address always carries `:port` — matching pvxs,
/// which prints `:port` whenever it is non-zero (`SockAddr::operator<<`,
/// `util.cpp:660-673`). A multicast entry additionally carries its
/// `,ttl@iface` modifiers, exactly as pvxs `SockEndpoint::operator<<` prints
/// them (`config.cpp:125-135`).
#[cfg(not(epics_embedded_target))]
fn format_endpoint(ep: &crate::config::env::Endpoint) -> String {
    use std::fmt::Write;
    let mut s = ep.addr.to_string();
    if ep.addr.ip().is_multicast() {
        if let Some(ttl) = ep.ttl {
            let _ = write!(s, ",{ttl}");
        }
        if let Some(iface) = &ep.iface {
            let _ = write!(s, "@{iface}");
        }
    }
    s
}

/// Render the `Effective config` block as a string (see
/// [`print_effective_config`]). Split out so the verbose output is
/// unit-testable without capturing process stdout.
///
/// Host-only: it renders the *post-`expand()`* config, and
/// [`crate::config::env::Config`] does not exist on `epics_embedded_target`.
#[cfg(not(epics_embedded_target))]
pub fn effective_config_string() -> String {
    use crate::config;
    use std::fmt::Write;

    // Build and expand the client config exactly as the client Context does,
    // so the readback shows the *effective* SEARCH configuration rather than
    // the raw environment. pvxs builds `ContextImpl::effective` as the env
    // config with `expand()` applied (`src/client.cpp:542-547`) and `-v` prints
    // that effective config (`tools/get.cpp:99-100` prints `ctxt.config()`).
    // `expand()` folds the auto-address-list broadcast fan-out into the
    // address list and clears the flag (`config.cpp:640-644`).
    let mut cfg = config::env::Config::from_client_env();
    cfg.expand();

    let addr_list = cfg
        .address_list
        .iter()
        .map(format_endpoint)
        .collect::<Vec<_>>()
        .join(" ");
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
    // pvxs prints the effective Config as a sorted `updateDefs()` map
    // (config.cpp:613-622, 653-663), so the keys come out alphabetically and use
    // their canonical `EPICS_PVA_*` names.
    let _ = writeln!(s, "  EPICS_PVA_ADDR_LIST={addr_list}");
    // After `expand()` the auto flag is always cleared (`config.cpp:643`),
    // so the effective config reports `NO` — pvxs prints the same, because
    // it too renders the post-`expand()` config.
    let _ = writeln!(
        s,
        "  EPICS_PVA_AUTO_ADDR_LIST={}",
        if cfg.auto_addr_list { "YES" } else { "NO" }
    );
    let _ = writeln!(s, "  EPICS_PVA_BROADCAST_PORT={}", config::broadcast_port());
    // pvxs keeps the connection timeout as a double and prints it in the
    // effective config (`config.cpp:211-227 parse_timeout`); the CLI
    // readback previously omitted `EPICS_PVA_CONN_TMO` entirely.
    let _ = writeln!(s, "  EPICS_PVA_CONN_TMO={}", config::conn_timeout_secs());
    // pvxs names this key `EPICS_PVA_INTF_ADDR_LIST`, not the non-pvxs
    // `interfaces=` the readback used before.
    let _ = writeln!(s, "  EPICS_PVA_INTF_ADDR_LIST={interfaces}");
    let _ = writeln!(s, "  EPICS_PVA_NAME_SERVERS={name_servers}");
    let _ = writeln!(s, "  EPICS_PVA_SERVER_PORT={}", config::server_port());
    s
}

/// Render the effective **server** (`EPICS_PVAS_*`) configuration block —
/// the server-side companion to `effective_config_string`, used by the
/// `pvinfo -D` host-troubleshooting report.
///
/// pvxs `target_information` prints both an "Effective Client config" and
/// an "Effective Server config" section under `pvxinfo -D`
/// (`src/describe.cpp:115-129`); the latter is `server::Config::fromEnv()`
/// after `expand()`, whose `operator<<` emits ONLY the `EPICS_PVAS_*`
/// variants of the def map, sorted (`src/config.cpp:474-545`). The
/// client-only `-v` block never shows these, so an operator debugging why
/// a *server* does or does not advertise on a network needs this distinct
/// block — which is why `-D` carries it and plain `-v` does not.
///
/// Keys and order mirror pvxs's sorted server def map. `EPICS_PVA_CONN_TMO`
/// is intentionally absent: pvxs sets it in `updateDefs` but its server
/// `operator<<` filters to the `EPICS_PVAS_` prefix, so it never appears in
/// the server section. Values come from the same resolved
/// [`crate::config`] server getters the PVA server itself reads — the
/// server-precedence `EPICS_PVAS_*`-first variants (`pvas_server_port`,
/// `server_broadcast_port`, `server_intf_addr_list`,
/// `server_beacon_addr_list`, `server_ignore_addr_list`,
/// `auto_beacon_addr_list_enabled`), not the client `server_port` whose
/// `EPICS_PVA_SERVER_PORT`-first precedence is the opposite
/// (`config.cpp:402-432` vs `568-578`).
///
/// Known gap (shared with the client block): the address lists are shown
/// as configured, not pvxs's post-`expand()` auto-beacon fan-out; that
/// expansion needs a server `Config`/`expand()` value in the config
/// module, which this CLI-scoped readback does not own.
pub fn effective_server_config_string() -> String {
    use crate::config;
    use std::fmt::Write;
    let beacon = config::server_beacon_addr_list()
        .iter()
        .map(|a| a.to_string())
        .collect::<Vec<_>>()
        .join(" ");
    let interfaces = config::server_intf_addr_list()
        .iter()
        .map(|a| a.to_string())
        .collect::<Vec<_>>()
        .join(" ");
    // pvxs joins ignore entries as `host:port` (`join_addr`); port 0 is
    // the wildcard "any port from this IP" match.
    let ignore = config::env::server_ignore_addr_list()
        .iter()
        .map(|(ip, port)| format!("{ip}:{port}"))
        .collect::<Vec<_>>()
        .join(" ");
    let mut s = String::new();
    let _ = writeln!(s, "Effective server config");
    let _ = writeln!(
        s,
        "  EPICS_PVAS_AUTO_BEACON_ADDR_LIST={}",
        if config::auto_beacon_addr_list_enabled() {
            "YES"
        } else {
            "NO"
        }
    );
    let _ = writeln!(s, "  EPICS_PVAS_BEACON_ADDR_LIST={beacon}");
    let _ = writeln!(
        s,
        "  EPICS_PVAS_BROADCAST_PORT={}",
        config::server_broadcast_port()
    );
    let _ = writeln!(s, "  EPICS_PVAS_IGNORE_ADDR_LIST={ignore}");
    let _ = writeln!(s, "  EPICS_PVAS_INTF_ADDR_LIST={interfaces}");
    // Server bind port uses pvxs server precedence (`EPICS_PVAS_SERVER_PORT`
    // first), distinct from the client block's `EPICS_PVA_SERVER_PORT`-first
    // `server_port`.
    let _ = writeln!(
        s,
        "  EPICS_PVAS_SERVER_PORT={}",
        config::env::pvas_server_port()
    );
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
/// pvxs's `SockEndpoint` ctor accepts both an interface name and an
/// interface IPv4 address, normalising the address back to a name
/// through `IfaceMap::name_of` (`config.cpp:76-80`). Resolving the
/// suffix only as a DNS host — as the cable tester previously did — made
/// `pvxvct -B 224.0.1.1@en0` fail unless `en0` happened to exist in DNS.
/// This is the single owner both tools share for `@iface` resolution.
pub fn resolve_iface_ipv4(spec: &str) -> Result<std::net::Ipv4Addr, String> {
    if let Ok(v4) = spec.parse::<std::net::Ipv4Addr>() {
        return Ok(v4);
    }
    #[cfg(all(unix, not(epics_embedded_target)))]
    {
        iface_name_to_ipv4(spec)
    }
    // RTEMS and VxWorks are both `cfg(unix)`, but RTEMS's newlib exposes no
    // `getifaddrs`/`ifaddrs`/`freeifaddrs` (design doc §8.1), and neither
    // does VxWorks's `libc` module, so a name cannot be resolved on either.
    // Same answer as any other non-getifaddrs host: say so and name the
    // workaround, rather than guessing an address.
    #[cfg(any(not(unix), epics_embedded_target))]
    {
        Err(format!(
            "interface-name override {spec:?} requires a host with \
             getifaddrs(3); pass the interface's IPv4 address instead"
        ))
    }
}

/// Look up an interface's first IPv4 address by name via `getifaddrs`.
#[cfg(all(unix, not(epics_embedded_target)))]
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

    /// R16-32 (same panic family as the env doubles): a huge-but-finite
    /// `-w` passed every helper's `is_finite() && > 0.0` guard and then
    /// panicked in `Duration::from_secs_f64` ("cannot convert float seconds
    /// to Duration"), aborting the tool. pvxs's `epicsEvent::wait(1e300)`
    /// just blocks; the port now saturates to the forever sentinel.
    #[test]
    fn out_of_range_w_saturates_instead_of_panicking() {
        let huge = 1e300;
        assert_eq!(timeout_duration(huge), TimeoutPolicy::FOREVER_SENTINEL);
        assert_eq!(wait_timeout_duration(huge), TimeoutPolicy::FOREVER_SENTINEL);
        assert_eq!(
            TimeoutPolicy::wait_or_forever(huge),
            TimeoutPolicy::Finite(TimeoutPolicy::FOREVER_SENTINEL)
        );
        // Just below the u64::MAX-seconds panic edge, the real value stands.
        assert_eq!(timeout_duration(1e18), Duration::from_secs_f64(1e18));
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
            "EPICS_PVA_BROADCAST_PORT=",
            // pvxs prints the connection timeout (config.cpp:211-227); the
            // readback must include it, not omit it as before.
            "EPICS_PVA_CONN_TMO=",
            // pvxs key name, NOT the non-pvxs `interfaces=` used before.
            "EPICS_PVA_INTF_ADDR_LIST=",
            "EPICS_PVA_NAME_SERVERS=",
            "EPICS_PVA_SERVER_PORT=",
        ] {
            assert!(s.contains(key), "missing {key:?} in:\n{s}");
        }
        // The non-pvxs `interfaces=` key must be gone.
        assert!(
            !s.contains("  interfaces="),
            "non-pvxs `interfaces=` key must be replaced: {s}"
        );
    }

    /// The `-v` block reports the *effective* (post-`expand()`) client
    /// config, not the raw environment: a configured `EPICS_PVA_ADDR_LIST`
    /// comes back with its effective UDP port, and `EPICS_PVA_AUTO_ADDR_LIST`
    /// always reads `NO` because `expand()` folds the broadcast fan-out into
    /// the list and clears the flag — exactly what pvxs prints, since it too
    /// renders `ctxt.config()` post-`expand()` (src/client.cpp:542-547,
    /// config.cpp:640-644). Serialised on `epics_env` because it mutates the
    /// process-global environment.
    #[test]
    #[serial_test::serial(epics_env)]
    fn effective_config_shows_expanded_addr_list_not_raw_env() {
        // SAFETY: std::env mutation is unsafe in edition 2024; the `epics_env`
        // serial guard makes it race-free.
        unsafe {
            std::env::set_var("EPICS_PVA_ADDR_LIST", "1.2.3.4");
            std::env::set_var("EPICS_PVA_AUTO_ADDR_LIST", "NO");
            std::env::remove_var("EPICS_PVA_BROADCAST_PORT");
        }
        let s = effective_config_string();
        // The configured address is rendered with its effective UDP port
        // (default 5076), proving the readback ran through `expand()` instead
        // of echoing the raw env string.
        assert!(
            s.contains("  EPICS_PVA_ADDR_LIST=1.2.3.4:5076\n"),
            "expanded address list with effective port expected, got:\n{s}"
        );
        // AUTO_ADDR_LIST=NO disables broadcast fan-out, so the effective list
        // is exactly the one configured address.
        assert!(s.contains("  EPICS_PVA_AUTO_ADDR_LIST=NO\n"), "got:\n{s}");

        // With AUTO_ADDR_LIST=YES the *raw* env says YES, but the effective
        // (post-`expand()`) config always reports NO — the parity fix.
        unsafe { std::env::set_var("EPICS_PVA_AUTO_ADDR_LIST", "YES") };
        let s = effective_config_string();
        assert!(
            s.contains("  EPICS_PVA_AUTO_ADDR_LIST=NO\n"),
            "post-expand AUTO_ADDR_LIST must read NO even when env=YES, got:\n{s}"
        );

        unsafe {
            std::env::remove_var("EPICS_PVA_ADDR_LIST");
            std::env::remove_var("EPICS_PVA_AUTO_ADDR_LIST");
        }
    }

    /// The `pvinfo -D` server block emits the pvxs `EPICS_PVAS_*` key set
    /// (`src/config.cpp:474-545`), and the client `-v` block stays
    /// client-only: it must carry no `EPICS_PVAS_` key. `EPICS_PVA_CONN_TMO`
    /// is deliberately absent from the server block — pvxs's server
    /// `operator<<` filters to the `EPICS_PVAS_` prefix.
    #[test]
    fn effective_server_config_emits_pvas_keys_and_v_stays_client_only() {
        let s = effective_server_config_string();
        assert!(s.starts_with("Effective server config\n"), "got: {s:?}");
        for key in [
            "EPICS_PVAS_AUTO_BEACON_ADDR_LIST=",
            "EPICS_PVAS_BEACON_ADDR_LIST=",
            "EPICS_PVAS_BROADCAST_PORT=",
            "EPICS_PVAS_IGNORE_ADDR_LIST=",
            "EPICS_PVAS_INTF_ADDR_LIST=",
            "EPICS_PVAS_SERVER_PORT=",
        ] {
            assert!(s.contains(key), "missing {key:?} in:\n{s}");
        }
        // pvxs filters CONN_TMO out of the server section (no PVAS prefix).
        assert!(
            !s.contains("EPICS_PVA_CONN_TMO"),
            "CONN_TMO must not appear in the server block: {s}"
        );
        // The client `-v` block is client-only: no EPICS_PVAS_ key.
        let client = effective_config_string();
        assert!(
            !client.contains("EPICS_PVAS_"),
            "the `-v` client block must not carry server keys: {client}"
        );
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
    /// address — the dual form pvxs's `SockEndpoint` ctor accepts
    /// (`config.cpp:76-80`). The loopback interface is `lo` on Linux
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

    /// The two `getopt` failure lines every pvAccess tool prints for
    /// itself (`pvget.cpp:353-362`), and the cases where C has no such
    /// line and clap's message must stand.
    mod c_option_wording {
        use super::super::c_option_error;
        use clap::{CommandFactory, Parser};

        /// The shape every pvAccess tool has: C short options, some of
        /// which the port also spells long (`pvget-rs -F` /
        /// `--format`).
        #[derive(Parser, Debug)]
        #[command(name = "pvget-rs")]
        struct Args {
            #[arg(short = 'w')]
            timeout: Option<f64>,
            #[arg(short = 'F', long = "format")]
            format: Option<String>,
            #[arg(short = 'm')]
            monitor: bool,
            names: Vec<String>,
        }

        fn c_error(argv: &[&str]) -> Option<String> {
            let argv: Vec<std::ffi::OsString> = argv.iter().map(Into::into).collect();
            let err = Args::try_parse_from(&argv).expect_err("must be a usage error");
            c_option_error(&err, &Args::command(), &argv)
        }

        #[test]
        fn an_unknown_short_option_names_the_letter_and_the_tool() {
            assert_eq!(
                c_error(&["pvget-rs", "-Z", "X"]).as_deref(),
                Some("Unrecognized option: '-Z'. ('pvget-rs -h' for help.)\n")
            );
        }

        #[test]
        fn a_short_option_missing_its_value_says_so() {
            assert_eq!(
                c_error(&["pvget-rs", "-w"]).as_deref(),
                Some("Option '-w' requires an argument. ('pvget-rs -h' for help.)\n")
            );
        }

        /// clap names `-F` by its long spelling in the error context;
        /// the letter is what C prints and what the user typed.
        #[test]
        fn a_short_option_that_also_has_a_long_form_still_names_the_letter() {
            assert_eq!(
                c_error(&["pvget-rs", "-F"]).as_deref(),
                Some("Option '-F' requires an argument. ('pvget-rs -h' for help.)\n")
            );
        }

        #[test]
        fn a_long_invocation_keeps_claps_message() {
            // `getopt` treats `--anything` as the end of options, so C
            // has no wording for a long form at all, and the letter
            // behind `--format` is not what this user typed.
            assert_eq!(c_error(&["pvget-rs", "--nope"]), None);
            assert_eq!(c_error(&["pvget-rs", "--format"]), None);
        }

        #[test]
        fn a_bad_value_keeps_claps_message() {
            // C's `:` arm is "missing", not "malformed"; a malformed
            // value is the tool's own `epicsParseDouble` diagnostic.
            assert_eq!(c_error(&["pvget-rs", "-w", "abc"]), None);
        }
    }
}
