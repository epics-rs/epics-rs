//! Environment-variable parsers for `EPICS_PVA_*` / `EPICS_PVAS_*`.
//!
//! Pure functions — they read `std::env::var(...)` directly so the
//! caller doesn't need to thread a Config struct. Where pvxs has
//! Config::fromEnv() that builds an internal config object, we expose
//! one helper per variable. Server-side helpers fall back to the
//! corresponding client-side variable when the `EPICS_PVAS_*` form is
//! not set, matching pvxs's `Config::server()` behavior.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;

/// Expand `$(VAR)` and `${VAR}` references in `input` against the
/// process environment. Mirrors pvxs `Config::expand()` (server.h:219).
/// Unrecognised refs (no env entry) substitute to an empty string —
/// matching the C IOC `dbLoadRecords` macro-expansion behaviour. A
/// literal `$` followed by non-`(`/`{` is preserved verbatim.
pub fn expand_dollar_vars(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '$' {
            let close = match chars.peek() {
                Some('(') => Some(')'),
                Some('{') => Some('}'),
                _ => None,
            };
            if let Some(end) = close {
                chars.next(); // consume '(' or '{'
                let mut name = String::new();
                let mut closed = false;
                for nc in chars.by_ref() {
                    if nc == end {
                        closed = true;
                        break;
                    }
                    name.push(nc);
                }
                if closed {
                    if let Ok(val) = std::env::var(&name) {
                        out.push_str(&val);
                    }
                    // Unset → empty (pvxs Config::expand parity).
                    continue;
                }
                // Unterminated $( … — emit as-is including the open
                // bracket so the caller's parser can fail loudly
                // rather than silently consuming text.
                out.push('$');
                out.push(if end == ')' { '(' } else { '{' });
                out.push_str(&name);
                continue;
            }
        }
        out.push(c);
    }
    out
}

/// Seed a name→value map of EPICS_PVA / EPICS_PVAS overrides into the
/// **process environment** so subsequent `*_from_env`-style reads pick
/// them up. A startup-only, `st.cmd`-style global seeder for callers
/// holding a dictionary of overrides (e.g. parsed from a config file).
///
/// This is deliberately **not** the pvxs `Config::applyDefs` analogue —
/// it mutates global state shared by every later reader, so it cannot
/// scope definitions to one client/server config. For pvxs-style
/// per-config definitions that leave the environment untouched, use
/// [`PvaConfigDefs::apply_defs`].
///
/// Semantics: for each `(name, value)` pair, sets `name=value` in the
/// process environment when `name` isn't already set, OR replaces the
/// existing value when `replace_existing` is true. Returns the number of
/// variables that were actually written. Keys are written verbatim with
/// no validation — caller is responsible for using real EPICS_PVA[S]_*
/// names.
pub fn seed_env_overrides(map: &HashMap<String, String>, replace_existing: bool) -> usize {
    let mut applied = 0usize;
    for (name, value) in map {
        let exists = std::env::var(name).is_ok();
        if exists && !replace_existing {
            continue;
        }
        // SAFETY: std::env::set_var is unsafe in the 2024 edition because
        // POSIX `setenv` isn't thread-safe relative to `getenv` from
        // other threads. PVA Config setup is called once at startup
        // before background tasks read env, matching pvxs Config
        // semantics — callers must uphold that single-threaded-setup
        // lifecycle.
        unsafe {
            std::env::set_var(name, value);
        }
        applied += 1;
    }
    applied
}

/// Per-config EPICS_PVA / EPICS_PVAS definitions, scoped to one
/// client/server configuration.
///
/// This is the Rust analogue of pvxs `Config::applyDefs(defs)`
/// (`src/pvxs/server.h:197-203`, `src/pvxs/client.h:1053-1059`,
/// implemented in `src/config.cpp:468-471` / `:607-610`): it captures the
/// supplied definitions **into this object** and leaves the process
/// environment unchanged. Two `PvaConfigDefs` built from two different
/// maps are fully independent — building one does not affect the other or
/// any [`seed_env_overrides`]-seeded global, so a single process can hold
/// several named client/server configs (pva2pva-style) without
/// cross-contamination.
///
/// Resolution precedence matches pvxs (explicit definitions win over the
/// ambient environment): [`PvaConfigDefs::get`] returns the scoped
/// definition when present, else falls back to the process environment.
/// Compose `get` with the pure parsers in this module
/// ([`parse_addr_list_with_port`], integer/`parse_bool` parsing) to derive
/// typed config values without reading global state for a scoped key.
#[derive(Debug, Clone, Default)]
pub struct PvaConfigDefs {
    defs: HashMap<String, String>,
}

impl PvaConfigDefs {
    /// Capture `map` as this config's definitions. Does **not** touch the
    /// process environment (pvxs `Config::applyDefs` contract). Keys are
    /// stored verbatim; the caller supplies real `EPICS_PVA[S]_*` names.
    pub fn apply_defs(map: &HashMap<String, String>) -> Self {
        Self { defs: map.clone() }
    }

    /// Resolve a variable name. A scoped definition takes precedence; when
    /// this config does not define `name`, fall back to the process
    /// environment (pvxs: explicit defs override the ambient env).
    pub fn get(&self, name: &str) -> Option<String> {
        match self.defs.get(name) {
            Some(v) => Some(v.clone()),
            None => std::env::var(name).ok(),
        }
    }

    /// True when `name` is defined by this config (independent of the
    /// process environment).
    pub fn contains(&self, name: &str) -> bool {
        self.defs.contains_key(name)
    }
}

/// From a synchronous-resolver iterator, pick the first IPv4 answer, else
/// the first IPv6. pvxs `SockAddr::setAddress` (util.cpp:530-540) applies
/// the same "we always prefer IPv4" rule; this stack additionally needs it
/// because `AsyncUdpV4` is IPv4-only and would reject a V6 send target.
/// Shared by every env address-list parser so the family cannot drift back
/// to dropping DNS hostnames at one site while resolving them at another.
fn pick_v4_preferred(iter: impl Iterator<Item = SocketAddr>) -> Option<SocketAddr> {
    let mut v4: Option<SocketAddr> = None;
    let mut v6: Option<SocketAddr> = None;
    for sa in iter {
        match sa {
            SocketAddr::V4(_) if v4.is_none() => v4 = Some(sa),
            SocketAddr::V6(_) if v6.is_none() => v6 = Some(sa),
            _ => {}
        }
    }
    v4.or(v6)
}

/// Parse a `EPICS_PVA_ADDR_LIST`-style string into a list of
/// `SocketAddr`, discarding any per-entry multicast modifiers. Entries are
/// **whitespace-separated** (pvxs `split_addr_into`, `config.cpp:151-169`);
/// each is an `<addr>[,ttl][@iface]` [`Endpoint`] whose address is kept.
/// Accepts plain IPs (gets `default_port` appended), `ip:port`, DNS
/// hostnames, and `hostname:port`; unresolvable entries are dropped with a
/// debug log.
///
/// The comma is **not** a separator — it is multicast-TTL syntax inside an
/// entry, matching pvxs. Callers that must honour the TTL / interface
/// modifiers on the UDP send path use [`parse_endpoints_with_port`]
/// instead.
pub fn parse_addr_list_with_port(env: &str, default_port: u16) -> Vec<SocketAddr> {
    parse_endpoints_with_port(env, default_port)
        .into_iter()
        .map(|e| e.addr)
        .collect()
}

/// A parsed address-list entry: a destination socket address plus the
/// optional multicast modifiers pvxs carries in a `SockEndpoint`
/// (`config.cpp:32-61`):
///
/// ```text
/// <IP46>
/// <IP46>,<ttl#>
/// <IP46>@ifacename
/// <IP46>,<ttl#>@ifacename
/// ```
///
/// The comma (multicast TTL) must precede the `@` (outgoing interface).
/// `ttl`/`iface` are only meaningful for multicast destinations: pvxs's
/// `operator<<` re-prints them only when `addr.isMCast()`, and the send
/// path applies `IP_MULTICAST_TTL` / `IP_MULTICAST_IF` only for multicast
/// (`evhelper.cpp:556-577`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    /// Resolved destination (IP + port).
    pub addr: SocketAddr,
    /// Multicast TTL (`,<n>` modifier). `None` = OS default. Applied only
    /// when `addr` is a multicast group.
    pub ttl: Option<u32>,
    /// Outgoing interface (`@name-or-ip` modifier), verbatim from the
    /// token. `None` = OS default route. Applied only for multicast.
    pub iface: Option<String>,
}

impl Endpoint {
    /// Parse one whitespace-delimited address-list token into an
    /// [`Endpoint`], appending `default_port` when the address carries no
    /// port. Mirrors pvxs `SockEndpoint::SockEndpoint` (`config.cpp:32-61`)
    /// for the `<addr>[,ttl][@iface]` grammar; the address itself resolves
    /// through the same IP / `host:port` / DNS path as the rest of the list
    /// (so a hostname endpoint still resolves). Returns `None` when the
    /// address fails to resolve or the grammar is violated (comma after
    /// `@`, unparseable TTL), logging at `debug` so an operator can spot
    /// the dropped token — matching the existing drop-with-log behaviour
    /// for unresolvable entries.
    pub fn parse(token: &str, default_port: u16) -> Option<Endpoint> {
        let token = token.trim();
        if token.is_empty() {
            return None;
        }
        let comma = token.find(',');
        let at = token.find('@');
        // pvxs: "comma expected before @" — a `@iface,ttl` ordering is a
        // syntax error, not a silent reinterpretation.
        if let (Some(c), Some(a)) = (comma, at) {
            if c > a {
                tracing::debug!(
                    token = %token,
                    "EPICS_PVA addr-list: comma after @ (expected addr,ttl@iface)"
                );
                return None;
            }
        }
        let (addr_str, ttl_str, iface) = match comma.or(at) {
            // No modifiers — the whole token is the address.
            None => (token, None, None),
            Some(first) => {
                let addr_str = &token[..first];
                let ttl_str = match (comma, at) {
                    (Some(c), Some(a)) => Some(&token[c + 1..a]),
                    (Some(c), None) => Some(&token[c + 1..]),
                    _ => None,
                };
                let iface = at.and_then(|a| {
                    let s = token[a + 1..].trim();
                    (!s.is_empty()).then(|| s.to_string())
                });
                (addr_str, ttl_str, iface)
            }
        };
        let ttl = match ttl_str {
            Some(s) => match s.trim().parse::<u32>() {
                Ok(v) => Some(v),
                Err(e) => {
                    tracing::debug!(
                        token = %token,
                        ttl = %s,
                        error = %e,
                        "EPICS_PVA addr-list: bad multicast TTL"
                    );
                    return None;
                }
            },
            None => None,
        };
        let addr = resolve_token_addr(addr_str.trim(), default_port)?;
        Some(Endpoint { addr, ttl, iface })
    }
}

impl From<SocketAddr> for Endpoint {
    /// A programmatically-supplied address carries no multicast modifiers
    /// (a `SocketAddr` cannot express `,ttl`/`@iface`), so it maps to a
    /// modifier-less endpoint. Lets callers that still build `Vec<SocketAddr>`
    /// (e.g. the gateway's own parser, test fixtures) feed the endpoint-typed
    /// beacon/destination lists with `.into()`.
    fn from(addr: SocketAddr) -> Self {
        Endpoint {
            addr,
            ttl: None,
            iface: None,
        }
    }
}

/// Resolve an `@iface` modifier — an interface NAME (`eth0`) or a literal
/// IPv4 address — to the interface's IPv4 address, for selecting the
/// outgoing multicast interface on the UDP send path. Mirrors pvxs
/// `IfaceMap::address_of` (`evhelper.cpp:872-879`): a name is looked up in
/// the live interface table; a dotted IPv4 address is accepted verbatim
/// (pvxs normalises a dotted iface address back to a name in the
/// `SockEndpoint` ctor, `config.cpp:76-79`, but the address itself is a
/// valid spec). Errs when the name has no IPv4 address so the caller can
/// fall back (best-effort) rather than silently mis-route.
pub fn resolve_iface_v4(spec: &str) -> Result<Ipv4Addr, String> {
    if let Ok(v4) = spec.parse::<Ipv4Addr>() {
        return Ok(v4);
    }
    let ifaces = if_addrs::get_if_addrs().map_err(|e| format!("get_if_addrs failed: {e}"))?;
    for iface in ifaces {
        if iface.name == spec {
            if let if_addrs::IfAddr::V4(v4) = iface.addr {
                return Ok(v4.ip);
            }
        }
    }
    Err(format!("interface {spec:?} has no IPv4 address"))
}

/// Resolve a single address token (no multicast modifiers) to a
/// `SocketAddr`, appending `default_port` when no port is present. Shared
/// by [`Endpoint::parse`] and [`parse_addr_list_with_port`].
///
/// P-6 (BUG_ARCHAEOLOGY libca a8e8d22c3): the previous parser only
/// accepted literal IPs, silently dropping every DNS hostname. C libca had
/// a 32-byte buffer truncation bug; we had a stricter "drop the whole
/// token" bug — same operator-visible symptom of "Empty PV search address
/// list" with no actionable error. Accepts a bracketed `[v6]:port`, a bare
/// IP, or a `host[:port]` / bare hostname resolved via DNS (IPv4 preferred
/// — this stack's `AsyncUdpV4` is IPv4-only and would reject a V6
/// destination at send time; macOS commonly orders `::1` before
/// `127.0.0.1`, so taking the first answer would silently drop unicast
/// SEARCH to localhost). Returns `None` (with a `debug` log) for an
/// unresolvable token.
fn resolve_token_addr(s: &str, default_port: u16) -> Option<SocketAddr> {
    use std::net::ToSocketAddrs;
    if s.is_empty() {
        return None;
    }
    if let Ok(sa) = s.parse::<SocketAddr>() {
        return Some(sa);
    }
    if let Ok(ip) = s.parse::<IpAddr>() {
        return Some(SocketAddr::new(ip, default_port));
    }
    let with_port = if s.contains(':') {
        s.to_string()
    } else {
        format!("{s}:{default_port}")
    };
    match with_port.to_socket_addrs() {
        Ok(iter) => pick_v4_preferred(iter).or_else(|| {
            tracing::debug!(token = %s, "EPICS_PVA addr-list: empty resolution");
            None
        }),
        Err(e) => {
            tracing::debug!(token = %s, error = %e, "EPICS_PVA addr-list: resolve failed");
            None
        }
    }
}

/// Endpoint-preserving variant of [`parse_addr_list_with_port`]: splits on
/// WHITESPACE only (pvxs `split_addr_into`, `config.cpp:151-169` — the
/// comma is multicast-TTL syntax, never a list separator) and keeps each
/// entry's multicast TTL / interface modifiers for the UDP send path.
pub fn parse_endpoints_with_port(env: &str, default_port: u16) -> Vec<Endpoint> {
    // PVA-466: pre-expand $(VAR) refs so callers can write
    // `EPICS_PVA_ADDR_LIST="$(IOC_HOST):5076"` (matching the dbLoad
    // macro-expansion convention).
    let env = expand_dollar_vars(env);
    env.split_whitespace()
        .filter_map(|tok| Endpoint::parse(tok, default_port))
        .collect()
}

/// Default-port variant using `EPICS_PVA_BROADCAST_PORT` (5076 fallback).
pub fn parse_addr_list(env: &str) -> Vec<SocketAddr> {
    parse_addr_list_with_port(env, broadcast_port())
}

/// Parse a PVA boolean env value, returning `None` for any value pvxs
/// would reject so the caller **preserves its default** rather than
/// collapsing an invalid string to `false`.
///
/// pvxs `parse_bool` (config.cpp:199-208) assigns the destination only
/// for `YES`/`1` (true) or `NO`/`0` (false); on any other value it logs
/// `"<name> invalid bool value (YES/NO)"` and leaves the destination at
/// its prior/default state. Collapsing e.g. `EPICS_PVA_AUTO_ADDR_LIST=maybe`
/// to `false` silently disabled discovery on a typo; returning `None`
/// keeps the documented default enabled, matching pvxs.
///
/// Accepted true values extend pvxs with the historically documented
/// `Y`/`TRUE`; the symmetric false set is `NO`/`N`/`0`/`FALSE`. Every
/// other value is invalid (`None` + a warning), never silently `false`.
fn parse_bool(name: &str, raw: &str) -> Option<bool> {
    match raw.trim().to_ascii_uppercase().as_str() {
        "YES" | "Y" | "1" | "TRUE" => Some(true),
        "NO" | "N" | "0" | "FALSE" => Some(false),
        _ => {
            tracing::warn!(
                var = name,
                value = raw,
                "invalid PVA boolean env value (expected YES/NO); keeping default"
            );
            None
        }
    }
}

/// Effective **client** UDP search/broadcast port from
/// `EPICS_PVA_BROADCAST_PORT` (default 5076).
///
/// pvxs (config.cpp:556-566) parses this into the client `udp_port` and
/// then explicitly rejects `udp_port == 0`: it logs "ignoring
/// EPICS_PVA_BROADCAST_PORT=0" and restores 5076, because a client SEARCH
/// targeting UDP port 0 can never reach a server. This helper is the
/// single owner of that rule for every client search destination —
/// limited broadcast, per-NIC broadcast, the `EPICS_PVA_ADDR_LIST`
/// default port, and the beacon-listener bind all read it. The
/// server-side `EPICS_PVAS_BROADCAST_PORT` path
/// ([`server_broadcast_port`]) keeps port 0 (pvxs allows a server random
/// bind/readback), so the zero rule lives here, not in the shared parse.
/// An unparseable value also falls back to 5076 (pvxs leaves `udp_port` at
/// its default on a parse error).
pub fn broadcast_port() -> u16 {
    match std::env::var("EPICS_PVA_BROADCAST_PORT")
        .ok()
        .and_then(|s| s.parse::<u16>().ok())
    {
        Some(0) => {
            tracing::warn!("ignoring EPICS_PVA_BROADCAST_PORT=0; using default 5076");
            5076
        }
        Some(p) => p,
        None => 5076,
    }
}

/// `EPICS_PVAS_BROADCAST_PORT` falling back to `EPICS_PVA_BROADCAST_PORT`,
/// returning `None` when neither variable is set. The presence-aware form
/// lets [`crate::server_native::PvaServerConfig::with_env`] preserve a
/// caller-supplied port when the env is silent (pvxs `PickOne`,
/// `config.cpp:397-437`).
pub fn server_broadcast_port_opt() -> Option<u16> {
    std::env::var("EPICS_PVAS_BROADCAST_PORT")
        .ok()
        .or_else(|| std::env::var("EPICS_PVA_BROADCAST_PORT").ok())
        .and_then(|s| s.parse().ok())
}

/// `EPICS_PVAS_BROADCAST_PORT` falling back to `EPICS_PVA_BROADCAST_PORT`.
pub fn server_broadcast_port() -> u16 {
    server_broadcast_port_opt().unwrap_or(5076)
}

/// Effective **client** default TCP destination port — the port a bare
/// (port-less) `EPICS_PVA_NAME_SERVERS` / address-list token resolves to.
///
/// pvxs `config.cpp:568-578` lets the client TCP port come from
/// `EPICS_PVAS_SERVER_PORT` when `EPICS_PVA_SERVER_PORT` is not set, so a
/// site that only configured the server-specific var still has a coherent
/// default. `Config::expand()` then normalizes an effective client TCP
/// port of zero back to the protocol default 5075 (`config.cpp:624-632`):
/// zero is a valid *server* ephemeral-bind request but never a usable
/// client destination, so `EPICS_PVA_SERVER_PORT=0` must not rewrite every
/// bare name-server token to `host:0`. The server bind port keeps zero —
/// see [`pvas_server_port`]. An explicit `host:0` in a name-server list is
/// preserved verbatim by [`parse_addr_list_with_port`]; only the *default*
/// port is expanded here.
pub fn server_port() -> u16 {
    let parsed = std::env::var("EPICS_PVA_SERVER_PORT")
        .ok()
        .or_else(|| std::env::var("EPICS_PVAS_SERVER_PORT").ok())
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(5075);
    // pvxs Config::expand(): a zero effective client TCP port → 5075.
    if parsed == 0 { 5075 } else { parsed }
}

/// server-side TCP port helper that mirrors pvxs
/// `config.cpp:402-408` `PickOne` precedence — server-specific
/// `EPICS_PVAS_SERVER_PORT` first, then shared `EPICS_PVA_SERVER_PORT`,
/// finally the compiled default. Pre-fix Rust read only the shared
/// variable for the server, so a pvxs-style deployment that set
/// `EPICS_PVAS_SERVER_PORT` was silently ignored and the Rust server
/// bound to 5075.
pub fn pvas_server_port() -> u16 {
    pvas_server_port_opt().unwrap_or(5075)
}

/// Presence-aware [`pvas_server_port`] — `None` when neither
/// `EPICS_PVAS_SERVER_PORT` nor `EPICS_PVA_SERVER_PORT` is set, so
/// `with_env` preserves a caller-supplied `tcp_port`.
pub fn pvas_server_port_opt() -> Option<u16> {
    std::env::var("EPICS_PVAS_SERVER_PORT")
        .ok()
        .or_else(|| std::env::var("EPICS_PVA_SERVER_PORT").ok())
        .and_then(|s| s.parse().ok())
}

/// `EPICS_PVA_AUTO_ADDR_LIST` — default YES. When truthy, the search
/// engine adds per-NIC broadcast addresses to the SEARCH targets list.
pub fn auto_addr_list_enabled() -> bool {
    match std::env::var("EPICS_PVA_AUTO_ADDR_LIST") {
        // Present-but-invalid preserves the default (`true`), matching
        // pvxs which leaves `autoAddrList` untouched on a bad value.
        Ok(v) => parse_bool("EPICS_PVA_AUTO_ADDR_LIST", &v).unwrap_or(true),
        Err(_) => true,
    }
}

/// `EPICS_PVAS_AUTO_BEACON_ADDR_LIST` — default YES. When truthy,
/// beacons fan out to each interface's limited broadcast (255.255.255.255
/// scoped to the NIC).
///
/// pvxs `config.cpp:426-431` falls back to
/// `EPICS_PVA_AUTO_ADDR_LIST` when the server-specific var is unset
/// so shared deployment config still drives the server's beacon
/// auto-discovery.
pub fn auto_beacon_addr_list_enabled() -> bool {
    auto_beacon_addr_list_enabled_opt().unwrap_or(true)
}

/// Presence-aware [`auto_beacon_addr_list_enabled`] — `None` when neither
/// `EPICS_PVAS_AUTO_BEACON_ADDR_LIST` nor `EPICS_PVA_AUTO_ADDR_LIST` is
/// set, so `with_env` preserves a caller-supplied `auto_beacon`.
pub fn auto_beacon_addr_list_enabled_opt() -> Option<bool> {
    // pvxs PickOne (config.cpp:430-432): first-present of the two vars
    // wins; the var name is carried into `parse_bool` for the diagnostic.
    // A present-but-invalid value yields `None` here so `with_env`
    // preserves the caller-supplied `auto_beacon` (matching pvxs's
    // leave-destination-unchanged-on-invalid contract), instead of the
    // old `false` collapse.
    let (name, raw) = std::env::var("EPICS_PVAS_AUTO_BEACON_ADDR_LIST")
        .map(|v| ("EPICS_PVAS_AUTO_BEACON_ADDR_LIST", v))
        .or_else(|_| {
            std::env::var("EPICS_PVA_AUTO_ADDR_LIST").map(|v| ("EPICS_PVA_AUTO_ADDR_LIST", v))
        })
        .ok()?;
    parse_bool(name, &raw)
}

/// `EPICS_PVAS_BEACON_PERIOD` — default 15s. Controls the *short*
/// burst-interval; see [`crate::server_native::runtime::PvaServerConfig`]
/// for the burst-then-slowdown semantics. Rust extension — pvxs has no
/// configurable beacon interval (fixed 15s/180s, `server.cpp:45-46`).
///
/// Returns a [`Duration`] built with [`Duration::from_secs_f64`] so a
/// sub-second positive request (`0.5`) is honored as 500ms rather than
/// truncated to zero. The 100ms floor is applied *after* the float
/// conversion, so every `0 < value` survives as a real delay instead of
/// collapsing into a `Duration::ZERO` emit-loop.
pub fn beacon_period() -> Duration {
    beacon_period_opt().unwrap_or(Duration::from_secs(15))
}

/// Presence-aware [`beacon_period`] — `None` when `EPICS_PVAS_BEACON_PERIOD`
/// is unset (or rejected as non-positive/non-finite), so `with_env`
/// preserves a caller-supplied `beacon_period`.
pub fn beacon_period_opt() -> Option<Duration> {
    std::env::var("EPICS_PVAS_BEACON_PERIOD")
        .ok()
        .and_then(|s| s.parse::<f64>().ok())
        // Reject negatives, NaN, infinity, and zero.
        .filter(|f| f.is_finite() && *f > 0.0)
        .map(Duration::from_secs_f64)
        .map(|d| d.max(Duration::from_millis(100)))
}

/// `EPICS_PVAS_BEACON_PERIOD_LONG` — explicit long-interval override.
/// `None` falls back to 12× the short interval (pvxs 15→180 ratio).
/// Sub-second precision preserved via [`Duration::from_secs_f64`] with a
/// post-conversion 100ms floor, same as [`beacon_period`].
pub fn beacon_period_long() -> Option<Duration> {
    std::env::var("EPICS_PVAS_BEACON_PERIOD_LONG")
        .ok()
        .and_then(|s| s.parse::<f64>().ok())
        .filter(|f| f.is_finite() && *f > 0.0)
        .map(Duration::from_secs_f64)
        .map(|d| d.max(Duration::from_millis(100)))
}

/// `EPICS_PVAS_MAX_CONNECTIONS` — server hard cap on simultaneous
/// client connections. Excess accept()s are immediately closed. Default
/// 1024.
pub fn max_connections() -> usize {
    max_connections_opt().unwrap_or(1024)
}

/// Presence-aware [`max_connections`] — `None` when
/// `EPICS_PVAS_MAX_CONNECTIONS` is unset or non-positive.
pub fn max_connections_opt() -> Option<usize> {
    std::env::var("EPICS_PVAS_MAX_CONNECTIONS")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .filter(|&v| v > 0)
}

/// `EPICS_PVAS_MAX_CHANNELS_PER_CONN` — server cap on channels created
/// by a single client connection. Default 256.
pub fn max_channels_per_connection() -> usize {
    max_channels_per_connection_opt().unwrap_or(256)
}

/// Presence-aware [`max_channels_per_connection`] — `None` when
/// `EPICS_PVAS_MAX_CHANNELS_PER_CONN` is unset or non-positive.
pub fn max_channels_per_connection_opt() -> Option<usize> {
    std::env::var("EPICS_PVAS_MAX_CHANNELS_PER_CONN")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .filter(|&v| v > 0)
}

/// `EPICS_PVAS_MAX_OPS_PER_CHANNEL` — server cap on concurrent
/// in-flight operations (GET / PUT / MONITOR / RPC INITs awaiting their
/// matching DESTROY) per single channel. Default 64. See
/// [`crate::server_native::runtime::PvaServerConfig::max_ops_per_channel`]
/// for rationale.
pub fn max_ops_per_channel() -> usize {
    max_ops_per_channel_opt().unwrap_or(64)
}

/// Presence-aware [`max_ops_per_channel`] — `None` when
/// `EPICS_PVAS_MAX_OPS_PER_CHANNEL` is unset or non-positive.
pub fn max_ops_per_channel_opt() -> Option<usize> {
    std::env::var("EPICS_PVAS_MAX_OPS_PER_CHANNEL")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .filter(|&v| v > 0)
}

/// `EPICS_PVA_CONN_TMO` — connection idle timeout (default 30s, pvxs
/// uses 30s for ECHO probe interval too). When the connection is idle
/// for this long, the client sends an ECHO; without a response within
/// the same window it declares the link dead.
///
/// Returned as `f64` so an operator's fractional seconds survive to the
/// TCP-timeout owners. pvxs keeps the configured value as a `double`
/// (`config.cpp:211-227 parse_timeout`) and each owner applies the
/// `tmoScale` (4/3) and the `enforceTimeout` floor (`config.cpp:373-391`);
/// truncating to integer seconds here would shorten e.g. `2.5` to `2`
/// before any owner scaled it.
pub fn conn_timeout_secs() -> f64 {
    conn_timeout_secs_opt().unwrap_or(30.0)
}

/// Presence-aware [`conn_timeout_secs`] — `None` when `EPICS_PVA_CONN_TMO`
/// is unset (or non-positive/non-finite), so `with_env` preserves a
/// caller-supplied `idle_timeout`. The 4/3 scale and the `>= 2s` clamp
/// belong to each effective-timeout owner (pvxs `parse_timeout` /
/// `enforceTimeout`), not to this parser, so the raw positive-finite
/// double is returned unscaled.
pub fn conn_timeout_secs_opt() -> Option<f64> {
    std::env::var("EPICS_PVA_CONN_TMO")
        .ok()
        .and_then(|s| s.parse::<f64>().ok())
        .filter(|f| f.is_finite() && *f > 0.0)
}

/// `EPICS_PVAS_SEND_TMO` — server-side per-write timeout (default 5s).
/// Floored at 0.1s so a misconfigured `0` doesn't make every send
/// instantly fail. See `PvaServerConfig::send_timeout` for full
/// rationale (stuck-client detection on non-blocking tokio sockets).
pub fn send_timeout_secs() -> f64 {
    send_timeout_secs_opt().unwrap_or(5.0)
}

/// Presence-aware [`send_timeout_secs`] — `None` when `EPICS_PVAS_SEND_TMO`
/// is unset (or non-positive/non-finite).
pub fn send_timeout_secs_opt() -> Option<f64> {
    std::env::var("EPICS_PVAS_SEND_TMO")
        .ok()
        .and_then(|s| s.parse::<f64>().ok())
        .filter(|v| v.is_finite() && *v > 0.0)
        .map(|v| v.max(0.1))
}

/// `EPICS_PVAS_TLS_HANDSHAKE_TMO` — server-side TLS handshake timeout
/// (default 10s). Without an upper bound on `TlsAcceptor::accept` a
/// peer that completes TCP but stalls during ClientHello holds a slot
/// in `max_connections` until OS keepalive reaps the half-open TCP
/// (~30s on default keepalive); coordinated peers can exhaust the
/// connection limit. Floored at 1.0s.
pub fn tls_handshake_timeout_secs() -> f64 {
    tls_handshake_timeout_secs_opt().unwrap_or(10.0)
}

/// Presence-aware [`tls_handshake_timeout_secs`] — `None` when
/// `EPICS_PVAS_TLS_HANDSHAKE_TMO` is unset (or non-positive/non-finite).
pub fn tls_handshake_timeout_secs_opt() -> Option<f64> {
    std::env::var("EPICS_PVAS_TLS_HANDSHAKE_TMO")
        .ok()
        .and_then(|s| s.parse::<f64>().ok())
        .filter(|v| v.is_finite() && *v > 0.0)
        .map(|v| v.max(1.0))
}

/// Non-pvxs fallback password for a server-side TLS keychain.
///
/// pvxs has no `*_TLS_KEYCHAIN_PASSWORD` variable: it sources the
/// PKCS#12 password solely from the `;password` suffix of the keychain
/// spec (`ossl.cpp:232-238`, see [`split_keychain_spec`]). This helper
/// is a Rust-only convenience consulted ONLY when the keychain spec
/// carries no inline `;` suffix. It reads
/// `EPICS_PVAS_TLS_KEYCHAIN_PASSWORD`, falling back to
/// `EPICS_PVA_TLS_KEYCHAIN_PASSWORD`. `$(VAR)` / `${VAR}` refs are
/// expanded (PVA-466) so operators can template
/// `EPICS_PVAS_TLS_KEYCHAIN_PASSWORD="$(SECRET_DIR)/..."` style
/// indirections. Returns `None` when neither variable is set; an empty
/// string is preserved as `Some("")` (a deliberately password-less
/// PKCS#12 is distinct from "unset").
pub fn server_tls_keychain_password() -> Option<String> {
    std::env::var("EPICS_PVAS_TLS_KEYCHAIN_PASSWORD")
        .or_else(|_| std::env::var("EPICS_PVA_TLS_KEYCHAIN_PASSWORD"))
        .ok()
        .map(|s| expand_dollar_vars(&s))
}

/// Keychain path for a server-side TLS endpoint.
///
/// Reads `EPICS_PVAS_TLS_KEYCHAIN`, falling back to `EPICS_PVA_TLS_KEYCHAIN`
/// when the server-specific form is unset — matching pvxs `Config::server()`
/// `pickone({"EPICS_PVAS_TLS_KEYCHAIN", "EPICS_PVA_TLS_KEYCHAIN"})`
/// (`config.cpp:497`), which takes the first variable present. A server
/// configured with only the shared `EPICS_PVA_TLS_KEYCHAIN` must still
/// enable TLS (pvxs does); reading the server-specific form alone left such
/// a server with TLS silently disabled. The client path stays
/// `EPICS_PVA_TLS_KEYCHAIN`-only (pvxs `config.cpp:672`). `$(VAR)` /
/// `${VAR}` refs are expanded (PVA-466 parity). Returns `None` when neither
/// variable is set.
pub fn server_tls_keychain() -> Option<String> {
    std::env::var("EPICS_PVAS_TLS_KEYCHAIN")
        .or_else(|_| std::env::var("EPICS_PVA_TLS_KEYCHAIN"))
        .ok()
        .map(|s| expand_dollar_vars(&s))
}

/// TLS option string for a server-side endpoint.
///
/// Reads `EPICS_PVAS_TLS_OPTIONS`, falling back to `EPICS_PVA_TLS_OPTIONS`
/// when the server-specific form is unset — matching pvxs `Config::server()`
/// `pickone({"EPICS_PVAS_TLS_OPTIONS", "EPICS_PVA_TLS_OPTIONS"})`
/// (`config.cpp:501`), which takes the first variable present and ignores
/// the other (no merge). An operator who sets only the server form (e.g.
/// `EPICS_PVAS_TLS_OPTIONS=client_cert=require`) must see it honoured;
/// reading the shared `EPICS_PVA_TLS_OPTIONS` alone silently dropped the
/// requirement and let the server accept certless clients (fail-open).
/// Options are not `$(VAR)`-expanded (pvxs parity; the string is option
/// tokens like `client_cert=require`, not a path). Empty when neither set.
pub fn server_tls_options() -> String {
    std::env::var("EPICS_PVAS_TLS_OPTIONS")
        .or_else(|_| std::env::var("EPICS_PVA_TLS_OPTIONS"))
        .unwrap_or_default()
}

/// Non-pvxs fallback password for a client-side TLS keychain.
///
/// pvxs sources the PKCS#12 password from the `;password` suffix of the
/// keychain spec (`ossl.cpp:232-238`, see [`split_keychain_spec`]), not
/// from a dedicated env var. This Rust-only convenience is consulted
/// ONLY when the keychain spec carries no inline `;` suffix. Reads
/// `EPICS_PVA_TLS_KEYCHAIN_PASSWORD`; `$(VAR)` / `${VAR}` refs are
/// expanded. Returns `None` when unset.
pub fn client_tls_keychain_password() -> Option<String> {
    std::env::var("EPICS_PVA_TLS_KEYCHAIN_PASSWORD")
        .ok()
        .map(|s| expand_dollar_vars(&s))
}

/// Split a TLS keychain spec into `(path, inline_password)` the way pvxs
/// `ossl.cpp:232-238` does: text before the FIRST `;` is the keychain
/// path, text after it (when a `;` is present) is the PKCS#12 password.
///
/// - `"cert.p12"`        → `("cert.p12", None)`     (no inline password)
/// - `"cert.p12;secret"` → `("cert.p12", Some("secret"))`
/// - `"cert.p12;"`       → `("cert.p12", Some(""))` (explicit empty password)
/// - `"cert.p12;a;b"`    → `("cert.p12", Some("a;b"))` (split at FIRST `;`)
///
/// `Some("")` (a deliberately password-less PKCS#12) is distinct from
/// `None` (no inline password — the caller may then consult the non-pvxs
/// `*_TLS_KEYCHAIN_PASSWORD` fallback). The inline suffix is the pvxs
/// source of truth and takes precedence at the call sites.
pub fn split_keychain_spec(spec: &str) -> (String, Option<String>) {
    match spec.split_once(';') {
        Some((path, password)) => (path.to_string(), Some(password.to_string())),
        None => (spec.to_string(), None),
    }
}

/// Parse `EPICS_PVA_NAME_SERVERS` into TCP socket addresses. Default
/// port 5075. Empty when the variable is unset.
pub fn name_servers() -> Vec<SocketAddr> {
    std::env::var("EPICS_PVA_NAME_SERVERS")
        .ok()
        .map(|s| parse_addr_list_with_port(&s, server_port()))
        .unwrap_or_default()
}

/// Parse `EPICS_PVA_ADDR_LIST` (or empty) — client-side unicast
/// SEARCH targets. Each entry pinned to `EPICS_PVA_BROADCAST_PORT`.
pub fn server_addr_list() -> Vec<SocketAddr> {
    std::env::var("EPICS_PVA_ADDR_LIST")
        .ok()
        .map(|s| parse_addr_list_with_port(&s, broadcast_port()))
        .unwrap_or_default()
}

/// Parse `EPICS_PVA_INTF_ADDR_LIST` — client-side interface bind list.
/// Empty = bind to 0.0.0.0 (default behaviour). `$(VAR)` / `${VAR}`
/// refs are expanded against the process env (PVA-466 parity).
pub fn list_intf_addresses() -> Vec<IpAddr> {
    std::env::var("EPICS_PVA_INTF_ADDR_LIST")
        .ok()
        .map(|s| {
            let s = expand_dollar_vars(&s);
            // pvxs splits address lists on WHITESPACE only (`split_addr_into`,
            // config.cpp:151-169); the comma is endpoint syntax, never a list
            // separator. Interface lists carry no multicast modifiers.
            s.split_whitespace()
                .filter_map(|t| t.trim().parse::<IpAddr>().ok())
                .collect()
        })
        .unwrap_or_default()
}

/// Parse `EPICS_PVAS_INTF_ADDR_LIST` — server-side interface bind list.
/// Falls back to `EPICS_PVA_INTF_ADDR_LIST` when unset; if both are
/// empty, returns an empty list (caller should bind 0.0.0.0).
/// Presence-aware [`server_intf_addr_list`] — `None` when neither
/// `EPICS_PVAS_INTF_ADDR_LIST` nor `EPICS_PVA_INTF_ADDR_LIST` is set, so
/// `with_env` preserves a caller-supplied `interfaces`.
pub fn server_intf_addr_list_opt() -> Option<Vec<IpAddr>> {
    let present = std::env::var("EPICS_PVAS_INTF_ADDR_LIST").is_ok()
        || std::env::var("EPICS_PVA_INTF_ADDR_LIST").is_ok();
    present.then(server_intf_addr_list)
}

pub fn server_intf_addr_list() -> Vec<IpAddr> {
    if let Ok(s) = std::env::var("EPICS_PVAS_INTF_ADDR_LIST") {
        // PVA-466: expand $(VAR) refs before tokenising.
        let s = expand_dollar_vars(&s);
        // Whitespace-only split (pvxs `split_addr_into`, config.cpp:151-169):
        // the comma is endpoint syntax, not a list separator.
        return s
            .split_whitespace()
            .filter_map(|t| {
                let t = t.trim();
                if t.is_empty() {
                    return None;
                }
                match t.parse::<IpAddr>() {
                    Ok(addr) => Some(addr),
                    Err(e) => {
                        tracing::warn!(
                            token = %t,
                            error = %e,
                            "EPICS_PVAS_INTF_ADDR_LIST: invalid IP address, skipping"
                        );
                        None
                    }
                }
            })
            .collect();
    }
    list_intf_addresses()
}

/// Parse `EPICS_PVAS_IGNORE_ADDR_LIST` — server-side blocklist. Each
/// entry pairs an IP with an optional port (`port == 0` matches any
/// port from that IP). Connections (TCP) and search packets (UDP)
/// from a matching peer are silently dropped. Mirrors pvxs
/// `Config::ignoreAddrs`. Default port for plain-IP entries is
/// `EPICS_PVAS_BROADCAST_PORT`, but the dropped-port match is
/// usually wildcard-by-zero anyway.
/// Presence-aware [`server_ignore_addr_list`] — `None` when
/// `EPICS_PVAS_IGNORE_ADDR_LIST` is unset, so `with_env` preserves a
/// caller-supplied `ignore_addrs`.
pub fn server_ignore_addr_list_opt() -> Option<Vec<(IpAddr, u16)>> {
    std::env::var("EPICS_PVAS_IGNORE_ADDR_LIST")
        .is_ok()
        .then(server_ignore_addr_list)
}

pub fn server_ignore_addr_list() -> Vec<(IpAddr, u16)> {
    let Ok(raw) = std::env::var("EPICS_PVAS_IGNORE_ADDR_LIST") else {
        return Vec::new();
    };
    // PVA-466: expand $(VAR) refs before tokenising.
    let raw = expand_dollar_vars(&raw);
    // Whitespace-only split (pvxs `split_addr_into`, config.cpp:151-169):
    // the comma is endpoint syntax, not a list separator.
    raw.split_whitespace()
        .filter_map(|s| {
            let s = s.trim();
            if s.is_empty() {
                return None;
            }
            resolve_ignore_entry(s)
        })
        .collect()
}

/// Resolve one `EPICS_PVAS_IGNORE_ADDR_LIST` token to `(IpAddr, port)`,
/// where `port == 0` is the wildcard "match any port from this IP". pvxs
/// parses this list through `split_addr_into(..., defaultPort=0)`
/// (`config.cpp:422`), which builds a `SockEndpoint` whose `setAddress`
/// resolves DNS hostnames (`util.cpp:444-540`). The previous Rust parser
/// only accepted numeric `SocketAddr`/`IpAddr`, so `ioc-host` or
/// `ioc-host:5076` was silently dropped and the peer never blocked.
fn resolve_ignore_entry(token: &str) -> Option<(IpAddr, u16)> {
    use std::net::ToSocketAddrs;
    // Numeric `ip:port` / `[ipv6]:port` — explicit port kept.
    if let Ok(sa) = token.parse::<SocketAddr>() {
        return Some((sa.ip(), sa.port()));
    }
    // Numeric bare IP (v4 or v6) — wildcard (0) port.
    if let Ok(ip) = token.parse::<IpAddr>() {
        return Some((ip, 0));
    }
    // `host:port` or bare hostname — synchronous DNS resolve, IPv4
    // preferred (pvxs `setAddress`). A bare host resolves against the
    // wildcard port `:0`, so its `port()` stays 0; `host:port` carries
    // its explicit port through resolution.
    let with_port = if token.contains(':') {
        token.to_string()
    } else {
        format!("{token}:0")
    };
    match with_port.to_socket_addrs() {
        Ok(iter) => match pick_v4_preferred(iter) {
            Some(sa) => Some((sa.ip(), sa.port())),
            None => {
                tracing::warn!(
                    token = %token,
                    "EPICS_PVAS_IGNORE_ADDR_LIST: host resolved to no addresses; entry ignored"
                );
                None
            }
        },
        Err(e) => {
            tracing::warn!(
                token = %token,
                error = %e,
                "EPICS_PVAS_IGNORE_ADDR_LIST: unresolvable entry ignored"
            );
            None
        }
    }
}

/// Parse `EPICS_PVAS_BEACON_ADDR_LIST` — explicit beacon destinations
/// (default port = `EPICS_PVAS_BROADCAST_PORT`). Falls back to empty
/// when unset (caller should auto-discover NIC broadcasts).
///
/// pvxs `config.cpp:426-431` falls back to
/// `EPICS_PVA_ADDR_LIST` when the server-specific list isn't set
/// (shared deployment config). Pre-fix Rust read only the
/// `EPICS_PVAS_*` var, so a site that listed beacon targets in
/// `EPICS_PVA_ADDR_LIST` had no beacons emitted.
pub fn server_beacon_addr_list() -> Vec<SocketAddr> {
    server_beacon_addr_list_opt().unwrap_or_default()
}

/// Presence-aware [`server_beacon_addr_list`] — `None` when neither
/// `EPICS_PVAS_BEACON_ADDR_LIST` nor `EPICS_PVA_ADDR_LIST` is set, so
/// `with_env` preserves caller-supplied `beacon_destinations`. Address-only
/// projection of [`server_beacon_endpoints_opt`].
pub fn server_beacon_addr_list_opt() -> Option<Vec<SocketAddr>> {
    server_beacon_endpoints_opt().map(|eps| eps.into_iter().map(|e| e.addr).collect())
}

/// Endpoint-preserving variant of [`server_beacon_addr_list_opt`]: keeps each
/// beacon destination's multicast TTL / interface modifiers so the UDP send
/// path can apply `IP_MULTICAST_TTL` / outgoing-interface selection per pvxs
/// (`evhelper.cpp:556-577`). Same env source + fallback
/// (`EPICS_PVAS_BEACON_ADDR_LIST` → `EPICS_PVA_ADDR_LIST`, `config.cpp:426-431`).
pub fn server_beacon_endpoints_opt() -> Option<Vec<Endpoint>> {
    std::env::var("EPICS_PVAS_BEACON_ADDR_LIST")
        .ok()
        .or_else(|| std::env::var("EPICS_PVA_ADDR_LIST").ok())
        .map(|s| parse_endpoints_with_port(&s, server_broadcast_port()))
}

/// Discover per-NIC IPv4 broadcast addresses. Used to fan SEARCH
/// requests / BEACONs across all subnets the host is attached to.
/// Skips loopback and interfaces without a broadcast address.
pub fn list_broadcast_addresses(port: u16) -> Vec<SocketAddr> {
    let mut out = Vec::new();
    let Ok(ifaces) = if_addrs::get_if_addrs() else {
        return out;
    };
    for iface in ifaces {
        if iface.is_loopback() {
            continue;
        }
        if let if_addrs::IfAddr::V4(v4) = iface.addr {
            if let Some(bcast) = v4.broadcast {
                out.push(SocketAddr::new(IpAddr::V4(bcast), port));
            }
        }
    }
    // Always include limited broadcast as a fallback.
    out.push(SocketAddr::new(IpAddr::V4(Ipv4Addr::BROADCAST), port));
    out
}

/// Like [`list_broadcast_addresses`] but restricted to the interfaces in
/// `interfaces` — the directed broadcast of each listed interface only.
///
/// This is the `EPICS_PVA*_INTF_ADDR_LIST` constraint on auto address
/// expansion: pvxs runs `expandAddrList` over `Config::interfaces`
/// (`config.cpp:624-648`), so a client constrained to a subset of
/// interfaces broadcasts only on those subnets. Rules:
///
/// * Empty `interfaces` → delegate to [`list_broadcast_addresses`] (the
///   all-interface default).
/// * A wildcard entry (`0.0.0.0`) means "every interface", so it also
///   delegates to the all-NIC enumeration.
/// * Otherwise: each listed interface contributes its own subnet
///   broadcast (looked up in the live interface table). The limited-
///   broadcast `255.255.255.255` fallback is appended **only** when at
///   least one listed interface is a real (non-loopback) NIC — so a
///   loopback-only list (`127.0.0.1`) yields an empty set and no
///   broadcast traffic leaves the host.
pub fn list_broadcast_addresses_on(interfaces: &[Ipv4Addr], port: u16) -> Vec<SocketAddr> {
    if interfaces.is_empty() || interfaces.iter().any(|ip| ip.is_unspecified()) {
        return list_broadcast_addresses(port);
    }
    let mut out = Vec::new();
    let mut any_non_loopback = false;
    if let Ok(ifaces) = if_addrs::get_if_addrs() {
        for want in interfaces {
            if want.is_loopback() {
                continue;
            }
            for iface in &ifaces {
                if let if_addrs::IfAddr::V4(v4) = &iface.addr {
                    if &v4.ip == want {
                        any_non_loopback = true;
                        if let Some(bcast) = v4.broadcast {
                            out.push(SocketAddr::new(IpAddr::V4(bcast), port));
                        }
                    }
                }
            }
        }
    }
    if any_non_loopback {
        out.push(SocketAddr::new(IpAddr::V4(Ipv4Addr::BROADCAST), port));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seed_env_overrides_writes_when_unset_and_skips_when_set_unless_replace() {
        unsafe {
            std::env::remove_var("EPICS_RS_AUDIT_DEFS_A");
            std::env::set_var("EPICS_RS_AUDIT_DEFS_B", "preset");
        }
        let mut map = HashMap::new();
        map.insert("EPICS_RS_AUDIT_DEFS_A".to_string(), "from-defs".to_string());
        map.insert(
            "EPICS_RS_AUDIT_DEFS_B".to_string(),
            "would-replace".to_string(),
        );
        let written = seed_env_overrides(&map, false);
        assert_eq!(written, 1, "B should not be replaced when not asked");
        assert_eq!(std::env::var("EPICS_RS_AUDIT_DEFS_A").unwrap(), "from-defs");
        assert_eq!(std::env::var("EPICS_RS_AUDIT_DEFS_B").unwrap(), "preset");
        // replace_existing = true overwrites
        let written2 = seed_env_overrides(&map, true);
        assert_eq!(written2, 2);
        assert_eq!(
            std::env::var("EPICS_RS_AUDIT_DEFS_B").unwrap(),
            "would-replace"
        );
        unsafe {
            std::env::remove_var("EPICS_RS_AUDIT_DEFS_A");
            std::env::remove_var("EPICS_RS_AUDIT_DEFS_B");
        }
    }

    #[test]
    fn pva_config_defs_are_independent_and_do_not_touch_env() {
        // pvxs `Config::applyDefs` contract: definitions are scoped to the
        // config object and the process environment is left unchanged, so
        // two configs built from two maps cannot contaminate each other.
        // Use a guaranteed-unset, uniquely-named key so the env-fallback
        // assertion is not racy with parallel tests.
        const KEY_ADDR: &str = "EPICS_PVA_ADDR_LIST";
        const KEY_PORT: &str = "EPICS_PVAS_SERVER_PORT";
        const UNSET: &str = "EPICS_RS_AUDIT_PVACFG_UNSET";
        // A uniquely-named scoped key that is also placed in the map, so
        // its absence from the env after apply_defs proves no global write.
        const AUDIT_KEY: &str = "EPICS_RS_AUDIT_PVACFG_SCOPED";
        unsafe {
            std::env::remove_var(UNSET);
            std::env::remove_var(AUDIT_KEY);
        }

        let mut a = HashMap::new();
        a.insert(KEY_ADDR.to_string(), "10.0.0.1".to_string());
        a.insert(KEY_PORT.to_string(), "5085".to_string());
        a.insert(AUDIT_KEY.to_string(), "scoped-only".to_string());
        let mut b = HashMap::new();
        b.insert(KEY_ADDR.to_string(), "192.168.1.1".to_string());
        b.insert(KEY_PORT.to_string(), "5095".to_string());

        let cfg_a = PvaConfigDefs::apply_defs(&a);
        let cfg_b = PvaConfigDefs::apply_defs(&b);

        // Independent: each config keeps its own definitions.
        assert_eq!(cfg_a.get(KEY_ADDR).as_deref(), Some("10.0.0.1"));
        assert_eq!(cfg_b.get(KEY_ADDR).as_deref(), Some("192.168.1.1"));
        assert_eq!(cfg_a.get(KEY_PORT).as_deref(), Some("5085"));
        assert_eq!(cfg_b.get(KEY_PORT).as_deref(), Some("5095"));

        // Scoped value parses through the existing pure parser without
        // reading global state.
        let addrs = parse_addr_list_with_port(&cfg_a.get(KEY_ADDR).unwrap(), 5076);
        assert_eq!(addrs.len(), 1);
        assert_eq!(addrs[0].ip().to_string(), "10.0.0.1");

        // apply_defs must NOT have written the process environment: a
        // scoped-only key resolves through the config but stays absent
        // from the env.
        assert_eq!(cfg_a.get(AUDIT_KEY).as_deref(), Some("scoped-only"));
        assert!(
            std::env::var(AUDIT_KEY).is_err(),
            "apply_defs must not seed the process environment"
        );

        // A key absent from the config falls back to the (here unset)
        // process environment → None.
        assert!(cfg_a.get(UNSET).is_none(), "unset key resolves to None");
        assert!(!cfg_a.contains(UNSET));
    }

    #[test]
    fn parse_bool_valid_true_false_invalid_none() {
        const N: &str = "EPICS_PVA_AUTO_ADDR_LIST";
        // True set (pvxs YES/1 + documented Y/TRUE extensions).
        assert_eq!(parse_bool(N, "YES"), Some(true));
        assert_eq!(parse_bool(N, "yes"), Some(true));
        assert_eq!(parse_bool(N, "Y"), Some(true));
        assert_eq!(parse_bool(N, "1"), Some(true));
        assert_eq!(parse_bool(N, "True"), Some(true));
        // False set (pvxs NO/0 + symmetric N/FALSE).
        assert_eq!(parse_bool(N, "NO"), Some(false));
        assert_eq!(parse_bool(N, "0"), Some(false));
        assert_eq!(parse_bool(N, "false"), Some(false));
        // Invalid → None so the caller preserves its default (pvxs leaves
        // the destination unchanged on an unrecognized value).
        assert_eq!(parse_bool(N, "maybe"), None);
        assert_eq!(parse_bool(N, ""), None);
    }

    #[test]
    fn auto_addr_list_invalid_value_preserves_default_enabled() {
        // pvxs: a misspelled EPICS_PVA_AUTO_ADDR_LIST keeps auto-addr ON;
        // the old truthy-collapse turned it OFF on any non-true string.
        // SAFETY: nextest runs each test in its own process, so mutating
        // the process environment here cannot race other tests.
        unsafe { std::env::set_var("EPICS_PVA_AUTO_ADDR_LIST", "maybe") };
        assert!(
            auto_addr_list_enabled(),
            "invalid value must preserve default true"
        );
        unsafe { std::env::set_var("EPICS_PVA_AUTO_ADDR_LIST", "NO") };
        assert!(!auto_addr_list_enabled(), "explicit NO disables");
        unsafe { std::env::remove_var("EPICS_PVA_AUTO_ADDR_LIST") };
        assert!(auto_addr_list_enabled(), "unset defaults to enabled");
    }

    #[test]
    fn broadcast_port_zero_falls_back_to_5076_client_only() {
        // pvxs ignores EPICS_PVA_BROADCAST_PORT=0 and restores 5076 for the
        // client (a SEARCH to UDP port 0 can never reach a server).
        // SAFETY: nextest runs each test in its own process.
        unsafe { std::env::set_var("EPICS_PVA_BROADCAST_PORT", "0") };
        assert_eq!(
            broadcast_port(),
            5076,
            "client port 0 must fall back to 5076"
        );
        unsafe { std::env::set_var("EPICS_PVA_BROADCAST_PORT", "5099") };
        assert_eq!(broadcast_port(), 5099, "explicit non-zero port honored");
        unsafe { std::env::set_var("EPICS_PVA_BROADCAST_PORT", "garbage") };
        assert_eq!(broadcast_port(), 5076, "unparseable falls back to 5076");
        // Server path is distinct: pvxs allows EPICS_PVAS_BROADCAST_PORT=0
        // for a server random bind/readback, so 0 is preserved there.
        unsafe { std::env::remove_var("EPICS_PVA_BROADCAST_PORT") };
        unsafe { std::env::set_var("EPICS_PVAS_BROADCAST_PORT", "0") };
        assert_eq!(
            server_broadcast_port(),
            0,
            "server port 0 is preserved (distinct from the client rule)"
        );
        unsafe { std::env::remove_var("EPICS_PVAS_BROADCAST_PORT") };
        assert_eq!(broadcast_port(), 5076, "unset defaults to 5076");
    }

    #[test]
    fn parse_addr_list_default_port() {
        let addrs = parse_addr_list_with_port("1.2.3.4 5.6.7.8:9876", 1234);
        assert_eq!(addrs.len(), 2);
        assert_eq!(addrs[0].port(), 1234);
        assert_eq!(addrs[1].port(), 9876);
    }

    /// pvxs `SockEndpoint` grammar (`config.cpp:32-61`): the four legal
    /// forms `<addr>`, `<addr>,<ttl>`, `<addr>@iface`, `<addr>,<ttl>@iface`,
    /// plus an explicit port carried in the address part.
    #[test]
    fn endpoint_parse_all_grammar_forms() {
        let plain = Endpoint::parse("224.0.2.3", 5076).expect("plain addr");
        assert_eq!(plain.addr, "224.0.2.3:5076".parse().unwrap());
        assert_eq!(plain.ttl, None);
        assert_eq!(plain.iface, None);

        let ttl = Endpoint::parse("224.0.2.3,5", 5076).expect("addr,ttl");
        assert_eq!(ttl.addr, "224.0.2.3:5076".parse().unwrap());
        assert_eq!(ttl.ttl, Some(5));
        assert_eq!(ttl.iface, None);

        let iface = Endpoint::parse("224.0.2.3@eth0", 5076).expect("addr@iface");
        assert_eq!(iface.ttl, None);
        assert_eq!(iface.iface.as_deref(), Some("eth0"));

        let both = Endpoint::parse("224.0.2.3,5@eth0", 5076).expect("addr,ttl@iface");
        assert_eq!(both.addr, "224.0.2.3:5076".parse().unwrap());
        assert_eq!(both.ttl, Some(5));
        assert_eq!(both.iface.as_deref(), Some("eth0"));

        // An explicit port in the address part survives the modifiers.
        let ported = Endpoint::parse("224.0.2.3:9999,5@eth0", 5076).expect("addr:port,ttl@iface");
        assert_eq!(ported.addr, "224.0.2.3:9999".parse().unwrap());
        assert_eq!(ported.ttl, Some(5));
        assert_eq!(ported.iface.as_deref(), Some("eth0"));
    }

    /// Grammar violations and empties are dropped (return `None`), matching
    /// pvxs which rejects rather than silently reinterpreting.
    #[test]
    fn endpoint_parse_rejects_malformed() {
        // pvxs: the comma (TTL) must precede the `@` (iface).
        assert_eq!(Endpoint::parse("224.0.2.3@eth0,5", 5076), None);
        // Non-numeric TTL.
        assert_eq!(Endpoint::parse("224.0.2.3,abc", 5076), None);
        // Empty address part.
        assert_eq!(Endpoint::parse(",5", 5076), None);
        assert_eq!(Endpoint::parse("@eth0", 5076), None);
        // Empty / whitespace token.
        assert_eq!(Endpoint::parse("", 5076), None);
        assert_eq!(Endpoint::parse("   ", 5076), None);
    }

    /// A programmatic `SocketAddr` carries no multicast modifiers, so the
    /// `From` shim that lets `Vec<SocketAddr>` callers feed the endpoint-typed
    /// beacon list must yield a modifier-less endpoint.
    #[test]
    fn endpoint_from_socket_addr_has_no_modifiers() {
        let sa: SocketAddr = "224.0.2.3:5076".parse().unwrap();
        let ep: Endpoint = sa.into();
        assert_eq!(ep.addr, sa);
        assert_eq!(ep.ttl, None);
        assert_eq!(ep.iface, None);
    }

    /// `resolve_iface_v4` accepts a literal IPv4 address verbatim (the egress
    /// NIC is later selected by source-bind), so a dotted `@iface` spec needs
    /// no interface-table lookup.
    #[test]
    fn resolve_iface_v4_literal_passthrough() {
        assert_eq!(
            resolve_iface_v4("192.168.7.42"),
            Ok(Ipv4Addr::new(192, 168, 7, 42))
        );
    }

    /// A name with no IPv4 address (here a non-existent interface) errors so
    /// the send path can fall back to the OS default route rather than
    /// silently mis-routing.
    #[test]
    fn resolve_iface_v4_unknown_name_errors() {
        assert!(resolve_iface_v4("nonexistent-iface-zzz").is_err());
    }

    /// The list splits on WHITESPACE only (pvxs `split_addr_into`); a comma
    /// is endpoint syntax, so it never multiplies the entry count.
    #[test]
    fn parse_endpoints_whitespace_only_split() {
        let eps = parse_endpoints_with_port("224.0.2.3,5@eth0 10.0.0.1 192.168.0.1:9", 5076);
        assert_eq!(eps.len(), 3, "three whitespace-separated endpoints");
        assert_eq!(eps[0].addr, "224.0.2.3:5076".parse().unwrap());
        assert_eq!(eps[0].ttl, Some(5));
        assert_eq!(eps[0].iface.as_deref(), Some("eth0"));
        assert_eq!(eps[1].addr, "10.0.0.1:5076".parse().unwrap());
        assert_eq!(eps[2].addr, "192.168.0.1:9".parse().unwrap());

        // `parse_addr_list_with_port` is the addr-only projection: same
        // count, modifiers discarded.
        let addrs = parse_addr_list_with_port("224.0.2.3,5@eth0 10.0.0.1", 5076);
        assert_eq!(addrs.len(), 2);
        assert_eq!(addrs[0], "224.0.2.3:5076".parse().unwrap());
    }

    #[test]
    fn expand_dollar_vars_substitutes_set_var() {
        // Use a long-named test var to avoid collisions with parallel
        // tests; expand should pull the value verbatim.
        // SAFETY: std::env::set_var is unsafe in 2024 edition; tests
        // run before any background task observes the variable.
        unsafe {
            std::env::set_var("EPICS_RS_AUDIT_X", "10.1.2.3");
        }
        let out = expand_dollar_vars("$(EPICS_RS_AUDIT_X):5076");
        assert_eq!(out, "10.1.2.3:5076");
        let out2 = expand_dollar_vars("${EPICS_RS_AUDIT_X}");
        assert_eq!(out2, "10.1.2.3");
        unsafe {
            std::env::remove_var("EPICS_RS_AUDIT_X");
        }
    }

    #[test]
    fn expand_dollar_vars_unset_collapses_to_empty() {
        // Unset variables match pvxs Config::expand semantics: empty.
        unsafe {
            std::env::remove_var("EPICS_RS_AUDIT_UNSET");
        }
        let out = expand_dollar_vars("a$(EPICS_RS_AUDIT_UNSET)b");
        assert_eq!(out, "ab");
    }

    #[test]
    fn expand_dollar_vars_preserves_unterminated() {
        // Unterminated $( … without closing should not eat the rest
        // of the string silently.
        let out = expand_dollar_vars("foo$(BAR");
        assert_eq!(out, "foo$(BAR");
    }

    #[test]
    fn list_broadcast_addresses_includes_limited_broadcast() {
        let bcasts = list_broadcast_addresses(5076);
        assert!(
            bcasts
                .iter()
                .any(|a| a.ip() == IpAddr::V4(Ipv4Addr::BROADCAST))
        );
    }

    /// PVA-466: $(VAR) expansion must apply to PVAS_INTF / PVA_INTF /
    /// PVAS_IGNORE addr lists, not only EPICS_PVA_ADDR_LIST. Without
    /// the wiring, ops who templated their st.cmd-style addr lists
    /// see literal `$(IFACE_IP)` tokens silently dropped as invalid.
    #[test]
    fn intf_and_ignore_addr_lists_expand_dollar_vars() {
        unsafe {
            std::env::set_var("EPICS_RS_AUDIT_IFACE", "127.0.0.1");
            std::env::set_var("EPICS_PVA_INTF_ADDR_LIST", "$(EPICS_RS_AUDIT_IFACE)");
            std::env::set_var("EPICS_PVAS_INTF_ADDR_LIST", "${EPICS_RS_AUDIT_IFACE}");
            std::env::set_var(
                "EPICS_PVAS_IGNORE_ADDR_LIST",
                "$(EPICS_RS_AUDIT_IFACE):5076",
            );
        }
        let client = list_intf_addresses();
        let server = server_intf_addr_list();
        let ignore = server_ignore_addr_list();
        assert_eq!(
            client,
            vec![IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1))],
            "EPICS_PVA_INTF_ADDR_LIST $(VAR) must expand"
        );
        assert_eq!(
            server,
            vec![IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1))],
            "EPICS_PVAS_INTF_ADDR_LIST ${{VAR}} must expand"
        );
        assert_eq!(
            ignore,
            vec![(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 5076)],
            "EPICS_PVAS_IGNORE_ADDR_LIST $(VAR) must expand"
        );
        unsafe {
            std::env::remove_var("EPICS_RS_AUDIT_IFACE");
            std::env::remove_var("EPICS_PVA_INTF_ADDR_LIST");
            std::env::remove_var("EPICS_PVAS_INTF_ADDR_LIST");
            std::env::remove_var("EPICS_PVAS_IGNORE_ADDR_LIST");
        }
    }

    /// PVA-40: numeric ignore-list tokens keep pvxs port semantics — a
    /// bare IP is a wildcard (port 0 matches any port from that IP); an
    /// `ip:port` token keeps its explicit port.
    #[test]
    fn resolve_ignore_entry_numeric_port_semantics() {
        assert_eq!(
            resolve_ignore_entry("192.168.1.1"),
            Some((IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1)), 0)),
            "bare IP must be wildcard port 0"
        );
        assert_eq!(
            resolve_ignore_entry("10.0.0.1:5075"),
            Some((IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)), 5075)),
            "ip:port must keep the explicit port"
        );
    }

    /// PVA-40 regression: a hostname token must be DNS-resolved instead of
    /// silently dropped. `localhost` is in every hosts file, so this needs
    /// no network. A bare hostname is a wildcard (port 0); `host:port`
    /// carries its explicit port through resolution.
    #[test]
    fn resolve_ignore_entry_resolves_hostnames() {
        let bare = resolve_ignore_entry("localhost").expect("localhost must resolve");
        assert!(
            bare.0.is_loopback(),
            "localhost must resolve to a loopback IP"
        );
        assert_eq!(bare.1, 0, "bare hostname must be wildcard port 0");

        let with_port =
            resolve_ignore_entry("localhost:5076").expect("localhost:5076 must resolve");
        assert!(
            with_port.0.is_loopback(),
            "localhost must resolve to a loopback IP"
        );
        assert_eq!(with_port.1, 5076, "host:port must keep the explicit port");
    }

    /// PVA-40: an unresolvable token is dropped (with a warning, not the
    /// old silent debug-less drop). `.invalid` is reserved by RFC 6761 to
    /// never resolve.
    #[test]
    fn resolve_ignore_entry_drops_unresolvable() {
        assert_eq!(resolve_ignore_entry("no-such-host.invalid"), None);
    }

    /// PVA-40 end-to-end: a server configured with a hostname in
    /// `EPICS_PVAS_IGNORE_ADDR_LIST` actually installs an ignore entry for
    /// that host (was an empty drop). Mixed numeric + hostname + invalid
    /// tokens: the invalid one is dropped, the rest resolve.
    #[test]
    fn server_ignore_addr_list_resolves_hostname_tokens() {
        // SAFETY: nextest runs each test in its own process; the env
        // mutation here cannot leak into a sibling test.
        unsafe {
            std::env::set_var(
                "EPICS_PVAS_IGNORE_ADDR_LIST",
                "localhost 10.0.0.1:5075 no-such-host.invalid",
            );
        }
        let list = server_ignore_addr_list();
        unsafe {
            std::env::remove_var("EPICS_PVAS_IGNORE_ADDR_LIST");
        }
        assert_eq!(
            list.len(),
            2,
            "invalid token dropped, two resolve: {list:?}"
        );
        assert!(
            list.iter().any(|(ip, port)| ip.is_loopback() && *port == 0),
            "hostname `localhost` must install a wildcard-port loopback entry: {list:?}"
        );
        assert!(
            list.contains(&(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)), 5075)),
            "numeric ip:port must survive alongside the resolved hostname: {list:?}"
        );
    }

    /// Env-driven server caps fall back to safe defaults when
    /// the var is unset, parses positive integers, and rejects 0 (which
    /// would otherwise let an operator misconfigure the cap to "no
    /// connections allowed").
    #[test]
    fn server_caps_fall_back_to_defaults_when_unset() {
        // SAFETY: clearing env vars is process-global; the helpers here
        // are pure functions with no parallel-test shared state, but we
        // still scope explicitly with remove_var so leftover values from
        // previous tests don't bleed in.
        unsafe {
            std::env::remove_var("EPICS_PVAS_MAX_CONNECTIONS");
            std::env::remove_var("EPICS_PVAS_MAX_CHANNELS_PER_CONN");
            std::env::remove_var("EPICS_PVAS_MAX_OPS_PER_CHANNEL");
        }
        assert_eq!(max_connections(), 1024);
        assert_eq!(max_channels_per_connection(), 256);
        assert_eq!(max_ops_per_channel(), 64);
    }

    /// server TLS options resolve PVAS-first, then shared PVA (pvxs
    /// `pickone({EPICS_PVAS_TLS_OPTIONS, EPICS_PVA_TLS_OPTIONS})`,
    /// config.cpp:501) — first present wins, no merge.
    #[test]
    #[serial_test::serial(epics_env)]
    fn server_tls_options_prefers_pvas_then_pva() {
        let prev_pvas = std::env::var("EPICS_PVAS_TLS_OPTIONS").ok();
        let prev_pva = std::env::var("EPICS_PVA_TLS_OPTIONS").ok();
        unsafe {
            std::env::set_var("EPICS_PVAS_TLS_OPTIONS", "client_cert=require");
            std::env::set_var("EPICS_PVA_TLS_OPTIONS", "client_cert=optional");
        }
        assert_eq!(server_tls_options(), "client_cert=require", "PVAS must win");
        unsafe {
            std::env::remove_var("EPICS_PVAS_TLS_OPTIONS");
        }
        assert_eq!(
            server_tls_options(),
            "client_cert=optional",
            "PVA fallback when PVAS unset"
        );
        unsafe {
            std::env::remove_var("EPICS_PVA_TLS_OPTIONS");
        }
        assert_eq!(server_tls_options(), "", "neither set → empty");
        unsafe {
            match prev_pvas {
                Some(v) => std::env::set_var("EPICS_PVAS_TLS_OPTIONS", v),
                None => std::env::remove_var("EPICS_PVAS_TLS_OPTIONS"),
            }
            match prev_pva {
                Some(v) => std::env::set_var("EPICS_PVA_TLS_OPTIONS", v),
                None => std::env::remove_var("EPICS_PVA_TLS_OPTIONS"),
            }
        }
    }

    /// server TLS keychain resolves PVAS-first, then shared PVA (pvxs
    /// `pickone({EPICS_PVAS_TLS_KEYCHAIN, EPICS_PVA_TLS_KEYCHAIN})`,
    /// config.cpp:497). The client path stays PVA-only (config.cpp:672).
    #[test]
    #[serial_test::serial(epics_env)]
    fn server_tls_keychain_prefers_pvas_then_pva() {
        let prev_pvas = std::env::var("EPICS_PVAS_TLS_KEYCHAIN").ok();
        let prev_pva = std::env::var("EPICS_PVA_TLS_KEYCHAIN").ok();
        unsafe {
            std::env::set_var("EPICS_PVAS_TLS_KEYCHAIN", "/srv/server.p12");
            std::env::set_var("EPICS_PVA_TLS_KEYCHAIN", "/cli/client.p12");
        }
        assert_eq!(server_tls_keychain().as_deref(), Some("/srv/server.p12"));
        unsafe {
            std::env::remove_var("EPICS_PVAS_TLS_KEYCHAIN");
        }
        assert_eq!(server_tls_keychain().as_deref(), Some("/cli/client.p12"));
        unsafe {
            std::env::remove_var("EPICS_PVA_TLS_KEYCHAIN");
        }
        assert_eq!(server_tls_keychain(), None);
        unsafe {
            match prev_pvas {
                Some(v) => std::env::set_var("EPICS_PVAS_TLS_KEYCHAIN", v),
                None => std::env::remove_var("EPICS_PVAS_TLS_KEYCHAIN"),
            }
            match prev_pva {
                Some(v) => std::env::set_var("EPICS_PVA_TLS_KEYCHAIN", v),
                None => std::env::remove_var("EPICS_PVA_TLS_KEYCHAIN"),
            }
        }
    }

    /// `split_keychain_spec` matches pvxs `ossl.cpp:232-238`: split at the
    /// FIRST `;`, path before, password after; no `;` → no inline password;
    /// trailing `;` → explicit empty password. One case per boundary.
    #[test]
    fn split_keychain_spec_matches_pvxs_first_semicolon() {
        // No `;`: whole spec is the path, no inline password.
        assert_eq!(
            split_keychain_spec("cert.p12"),
            ("cert.p12".to_string(), None)
        );
        // `;secret`: path + non-empty password.
        assert_eq!(
            split_keychain_spec("cert.p12;secret"),
            ("cert.p12".to_string(), Some("secret".to_string()))
        );
        // Trailing `;`: explicit empty password (distinct from None).
        assert_eq!(
            split_keychain_spec("cert.p12;"),
            ("cert.p12".to_string(), Some(String::new()))
        );
        // Multiple `;`: split at the FIRST only; the rest is the password
        // verbatim (pvxs find_first_of + substr(sep+1)).
        assert_eq!(
            split_keychain_spec("cert.p12;a;b"),
            ("cert.p12".to_string(), Some("a;b".to_string()))
        );
        // Leading `;`: empty path, password is the remainder (pvxs would
        // then fail to open the empty path — we preserve the same split).
        assert_eq!(
            split_keychain_spec(";onlypw"),
            (String::new(), Some("onlypw".to_string()))
        );
    }

    /// `EPICS_PVA_CONN_TMO` keeps fractional seconds. pvxs parses it as a
    /// `double` (`config.cpp:211-227`); the effective-timeout owners apply
    /// the 4/3 scale + floor on that double. Truncating to integer seconds
    /// here (the pre-fix `as u64`) shortened e.g. `2.5` to `2` before any
    /// owner scaled it.
    #[test]
    #[serial_test::serial(epics_env)]
    fn conn_tmo_preserves_fractional_seconds() {
        let prev = std::env::var("EPICS_PVA_CONN_TMO").ok();
        unsafe {
            std::env::set_var("EPICS_PVA_CONN_TMO", "2.5");
        }
        assert_eq!(
            conn_timeout_secs_opt(),
            Some(2.5),
            "fractional CONN_TMO must survive un-truncated"
        );
        assert_eq!(conn_timeout_secs(), 2.5);
        unsafe {
            std::env::remove_var("EPICS_PVA_CONN_TMO");
        }
        assert_eq!(conn_timeout_secs_opt(), None, "unset → None");
        assert_eq!(conn_timeout_secs(), 30.0, "unset → 30s default");
        unsafe {
            match prev {
                Some(v) => std::env::set_var("EPICS_PVA_CONN_TMO", v),
                None => std::env::remove_var("EPICS_PVA_CONN_TMO"),
            }
        }
    }

    /// `EPICS_PVA_SERVER_PORT=0` is a valid server ephemeral-bind request
    /// but never a usable client TCP destination, so the client default
    /// port expands a zero back to 5075 (pvxs `Config::expand()`,
    /// config.cpp:624-632). An explicit non-zero value and the unset
    /// default are unchanged.
    #[test]
    #[serial_test::serial(epics_env)]
    fn server_port_zero_expands_to_protocol_default() {
        let prev_pva = std::env::var("EPICS_PVA_SERVER_PORT").ok();
        let prev_pvas = std::env::var("EPICS_PVAS_SERVER_PORT").ok();
        unsafe {
            std::env::remove_var("EPICS_PVAS_SERVER_PORT");
            std::env::set_var("EPICS_PVA_SERVER_PORT", "0");
        }
        assert_eq!(server_port(), 5075, "zero must expand to the 5075 default");
        unsafe {
            std::env::set_var("EPICS_PVA_SERVER_PORT", "1234");
        }
        assert_eq!(server_port(), 1234, "explicit non-zero port is preserved");
        unsafe {
            std::env::remove_var("EPICS_PVA_SERVER_PORT");
        }
        assert_eq!(server_port(), 5075, "unset → 5075 default");
        unsafe {
            match prev_pva {
                Some(v) => std::env::set_var("EPICS_PVA_SERVER_PORT", v),
                None => std::env::remove_var("EPICS_PVA_SERVER_PORT"),
            }
            match prev_pvas {
                Some(v) => std::env::set_var("EPICS_PVAS_SERVER_PORT", v),
                None => std::env::remove_var("EPICS_PVAS_SERVER_PORT"),
            }
        }
    }

    /// With `EPICS_PVA_SERVER_PORT=0`, a bare `EPICS_PVA_NAME_SERVERS`
    /// token must resolve to the expanded default port 5075, not `:0`; an
    /// explicit `host:port` token keeps its literal port; an empty list
    /// yields no addresses.
    #[test]
    #[serial_test::serial(epics_env)]
    fn name_servers_bare_token_uses_expanded_port_under_server_port_zero() {
        let prev_pva = std::env::var("EPICS_PVA_SERVER_PORT").ok();
        let prev_pvas = std::env::var("EPICS_PVAS_SERVER_PORT").ok();
        let prev_ns = std::env::var("EPICS_PVA_NAME_SERVERS").ok();
        unsafe {
            std::env::remove_var("EPICS_PVAS_SERVER_PORT");
            std::env::set_var("EPICS_PVA_SERVER_PORT", "0");
            std::env::set_var("EPICS_PVA_NAME_SERVERS", "127.0.0.1");
        }
        let bare = name_servers();
        assert_eq!(
            bare.iter().map(SocketAddr::port).collect::<Vec<_>>(),
            vec![5075],
            "bare name-server token must use the expanded default port, not 0"
        );

        unsafe {
            std::env::set_var("EPICS_PVA_NAME_SERVERS", "127.0.0.1:9876");
        }
        let explicit = name_servers();
        assert_eq!(
            explicit.iter().map(SocketAddr::port).collect::<Vec<_>>(),
            vec![9876],
            "an explicit name-server port is preserved verbatim"
        );

        unsafe {
            std::env::remove_var("EPICS_PVA_NAME_SERVERS");
        }
        assert!(
            name_servers().is_empty(),
            "no name-server list → no addresses"
        );

        unsafe {
            match prev_pva {
                Some(v) => std::env::set_var("EPICS_PVA_SERVER_PORT", v),
                None => std::env::remove_var("EPICS_PVA_SERVER_PORT"),
            }
            match prev_pvas {
                Some(v) => std::env::set_var("EPICS_PVAS_SERVER_PORT", v),
                None => std::env::remove_var("EPICS_PVAS_SERVER_PORT"),
            }
            match prev_ns {
                Some(v) => std::env::set_var("EPICS_PVA_NAME_SERVERS", v),
                None => std::env::remove_var("EPICS_PVA_NAME_SERVERS"),
            }
        }
    }
}
