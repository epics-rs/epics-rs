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

/// Parse a `EPICS_PVA_ADDR_LIST`-style string (comma/whitespace
/// separated) into a list of `SocketAddr`. Accepts plain IPs (gets
/// `default_port` appended), `ip:port`, DNS hostnames (resolves via
/// `ToSocketAddrs`), and `hostname:port`. Unresolvable entries are
/// dropped with a debug log so operators can spot the silent miss.
///
/// P-6 (BUG_ARCHAEOLOGY libca a8e8d22c3): the previous parser only
/// accepted literal IPs, silently dropping every DNS hostname. C
/// libca had a 32-byte buffer truncation bug; we had a stricter
/// "drop the whole token" bug — same operator-visible symptom of
/// "Empty PV search address list" with no actionable error.
pub fn parse_addr_list_with_port(env: &str, default_port: u16) -> Vec<SocketAddr> {
    use std::net::ToSocketAddrs;
    // PVA-466: pre-expand $(VAR) refs so callers can write
    // `EPICS_PVA_ADDR_LIST="$(IOC_HOST):5076"` (matching the dbLoad
    // macro-expansion convention).
    let env = expand_dollar_vars(env);
    let env = env.as_str();
    env.split(|c: char| c == ',' || c.is_whitespace())
        .filter_map(|s| {
            let s = s.trim();
            if s.is_empty() {
                return None;
            }
            // 1. Bracketed IPv6 with port: `[::1]:5064`.
            if let Ok(sa) = s.parse::<SocketAddr>() {
                return Some(sa);
            }
            // 2. Bare IP (v4 or v6) — append default port.
            if let Ok(ip) = s.parse::<IpAddr>() {
                return Some(SocketAddr::new(ip, default_port));
            }
            // 3. `host:port` or bare hostname — synchronous DNS
            //    resolve via ToSocketAddrs. Prefer the first IPv4
            //    answer over IPv6: this stack's `AsyncUdpV4` is
            //    IPv4-only and would reject a V6 destination at
            //    send time. macOS commonly orders IPv6 ahead of
            //    IPv4 (e.g. `localhost` → `::1` before `127.0.0.1`)
            //    so taking the first answer unconditionally would
            //    silently drop unicast SEARCH to localhost.
            let with_port = if s.contains(':') {
                s.to_string()
            } else {
                format!("{s}:{default_port}")
            };
            match with_port.to_socket_addrs() {
                Ok(iter) => {
                    let mut v4: Option<SocketAddr> = None;
                    let mut v6: Option<SocketAddr> = None;
                    for sa in iter {
                        match sa {
                            SocketAddr::V4(_) if v4.is_none() => v4 = Some(sa),
                            SocketAddr::V6(_) if v6.is_none() => v6 = Some(sa),
                            _ => {}
                        }
                    }
                    v4.or(v6).or_else(|| {
                        tracing::debug!(token = %s, "EPICS_PVA addr-list: empty resolution");
                        None
                    })
                }
                Err(e) => {
                    tracing::debug!(token = %s, error = %e, "EPICS_PVA addr-list: resolve failed");
                    None
                }
            }
        })
        .collect()
}

/// Default-port variant using `EPICS_PVA_BROADCAST_PORT` (5076 fallback).
pub fn parse_addr_list(env: &str) -> Vec<SocketAddr> {
    parse_addr_list_with_port(env, broadcast_port())
}

/// Truthy parsing for `YES/NO` strings — pvxs accepts `YES`, `Y`, `1`,
/// `TRUE` (case-insensitive). Everything else is `NO`.
fn parse_bool(s: &str) -> bool {
    let v = s.trim().to_ascii_uppercase();
    matches!(v.as_str(), "YES" | "Y" | "1" | "TRUE")
}

/// `EPICS_PVA_BROADCAST_PORT` (default 5076).
pub fn broadcast_port() -> u16 {
    std::env::var("EPICS_PVA_BROADCAST_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(5076)
}

/// `EPICS_PVAS_BROADCAST_PORT` falling back to `EPICS_PVA_BROADCAST_PORT`.
pub fn server_broadcast_port() -> u16 {
    std::env::var("EPICS_PVAS_BROADCAST_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(broadcast_port)
}

/// `EPICS_PVA_SERVER_PORT` (default 5075).
///
/// pvxs `config.cpp:568-578` lets the client TCP port come
/// from `EPICS_PVAS_SERVER_PORT` when `EPICS_PVA_SERVER_PORT` is not
/// set, so a site that only configured the server-specific var still
/// has a coherent default for client name-server lookups.
pub fn server_port() -> u16 {
    std::env::var("EPICS_PVA_SERVER_PORT")
        .ok()
        .or_else(|| std::env::var("EPICS_PVAS_SERVER_PORT").ok())
        .and_then(|s| s.parse().ok())
        .unwrap_or(5075)
}

/// server-side TCP port helper that mirrors pvxs
/// `config.cpp:402-408` `PickOne` precedence — server-specific
/// `EPICS_PVAS_SERVER_PORT` first, then shared `EPICS_PVA_SERVER_PORT`,
/// finally the compiled default. Pre-fix Rust read only the shared
/// variable for the server, so a pvxs-style deployment that set
/// `EPICS_PVAS_SERVER_PORT` was silently ignored and the Rust server
/// bound to 5075.
pub fn pvas_server_port() -> u16 {
    std::env::var("EPICS_PVAS_SERVER_PORT")
        .ok()
        .or_else(|| std::env::var("EPICS_PVA_SERVER_PORT").ok())
        .and_then(|s| s.parse().ok())
        .unwrap_or(5075)
}

/// `EPICS_PVA_AUTO_ADDR_LIST` — default YES. When truthy, the search
/// engine adds per-NIC broadcast addresses to the SEARCH targets list.
pub fn auto_addr_list_enabled() -> bool {
    match std::env::var("EPICS_PVA_AUTO_ADDR_LIST") {
        Ok(v) => parse_bool(&v),
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
    let v = std::env::var("EPICS_PVAS_AUTO_BEACON_ADDR_LIST")
        .ok()
        .or_else(|| std::env::var("EPICS_PVA_AUTO_ADDR_LIST").ok());
    match v {
        Some(s) => parse_bool(&s),
        None => true,
    }
}

/// `EPICS_PVAS_BEACON_PERIOD` — default 15s. Controls the *short*
/// burst-interval; see [`crate::server_native::runtime::PvaServerConfig`]
/// for the burst-then-slowdown semantics.
pub fn beacon_period_secs() -> u64 {
    std::env::var("EPICS_PVAS_BEACON_PERIOD")
        .ok()
        .and_then(|s| s.parse::<f64>().ok())
        // Reject negatives, NaN, infinity, and zero — `f as u64` would
        // saturate to 0 silently, producing `Duration::ZERO` and a
        // beacon emit-loop that spins at memory bandwidth.
        .filter(|f| f.is_finite() && *f > 0.0)
        .map(|f| f.max(0.1) as u64)
        .unwrap_or(15)
}

/// `EPICS_PVAS_BEACON_PERIOD_LONG` — explicit long-interval override.
/// `None` falls back to 12× the short interval (pvxs 15→180 ratio).
pub fn beacon_period_long_secs() -> Option<u64> {
    std::env::var("EPICS_PVAS_BEACON_PERIOD_LONG")
        .ok()
        .and_then(|s| s.parse::<f64>().ok())
        .filter(|f| f.is_finite() && *f > 0.0)
        .map(|f| f.max(0.1) as u64)
}

/// `EPICS_PVAS_MAX_CONNECTIONS` — server hard cap on simultaneous
/// client connections. Excess accept()s are immediately closed. Default
/// 1024.
pub fn max_connections() -> usize {
    std::env::var("EPICS_PVAS_MAX_CONNECTIONS")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .filter(|&v| v > 0)
        .unwrap_or(1024)
}

/// `EPICS_PVAS_MAX_CHANNELS_PER_CONN` — server cap on channels created
/// by a single client connection. Default 256.
pub fn max_channels_per_connection() -> usize {
    std::env::var("EPICS_PVAS_MAX_CHANNELS_PER_CONN")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .filter(|&v| v > 0)
        .unwrap_or(256)
}

/// `EPICS_PVAS_MAX_OPS_PER_CHANNEL` — server cap on concurrent
/// in-flight operations (GET / PUT / MONITOR / RPC INITs awaiting their
/// matching DESTROY) per single channel. Default 64. See
/// [`crate::server_native::runtime::PvaServerConfig::max_ops_per_channel`]
/// for rationale.
pub fn max_ops_per_channel() -> usize {
    std::env::var("EPICS_PVAS_MAX_OPS_PER_CHANNEL")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .filter(|&v| v > 0)
        .unwrap_or(64)
}

/// `EPICS_PVA_CONN_TMO` — connection idle timeout (default 30s, pvxs
/// uses 30s for ECHO probe interval too). When the connection is idle
/// for this long, the client sends an ECHO; without a response within
/// the same window it declares the link dead.
pub fn conn_timeout_secs() -> u64 {
    std::env::var("EPICS_PVA_CONN_TMO")
        .ok()
        .and_then(|s| s.parse::<f64>().ok())
        .filter(|f| f.is_finite() && *f > 0.0)
        .map(|f| f.max(1.0) as u64)
        .unwrap_or(30)
}

/// `EPICS_PVAS_SEND_TMO` — server-side per-write timeout (default 5s).
/// Floored at 0.1s so a misconfigured `0` doesn't make every send
/// instantly fail. See `PvaServerConfig::send_timeout` for full
/// rationale (stuck-client detection on non-blocking tokio sockets).
pub fn send_timeout_secs() -> f64 {
    std::env::var("EPICS_PVAS_SEND_TMO")
        .ok()
        .and_then(|s| s.parse::<f64>().ok())
        .filter(|v| v.is_finite() && *v > 0.0)
        .map(|v| v.max(0.1))
        .unwrap_or(5.0)
}

/// `EPICS_PVAS_TLS_HANDSHAKE_TMO` — server-side TLS handshake timeout
/// (default 10s). Without an upper bound on `TlsAcceptor::accept` a
/// peer that completes TCP but stalls during ClientHello holds a slot
/// in `max_connections` until OS keepalive reaps the half-open TCP
/// (~30s on default keepalive); coordinated peers can exhaust the
/// connection limit. Floored at 1.0s.
pub fn tls_handshake_timeout_secs() -> f64 {
    std::env::var("EPICS_PVAS_TLS_HANDSHAKE_TMO")
        .ok()
        .and_then(|s| s.parse::<f64>().ok())
        .filter(|v| v.is_finite() && *v > 0.0)
        .map(|v| v.max(1.0))
        .unwrap_or(10.0)
}

/// Keychain password for a server-side TLS keychain.
///
/// Reads `EPICS_PVAS_TLS_KEYCHAIN_PASSWORD`, falling back to
/// `EPICS_PVA_TLS_KEYCHAIN_PASSWORD` when the server-specific form is
/// unset — matching pvxs's `Config::server()` env fallback chain.
/// `$(VAR)` / `${VAR}` refs are expanded (PVA-466 parity) so operators
/// can template `EPICS_PVAS_TLS_KEYCHAIN_PASSWORD="$(SECRET_DIR)/..."`
/// style indirections. Returns `None` when neither variable is set;
/// an empty string is preserved as `Some("")` (a deliberately
/// password-less PKCS#12 is distinct from "unset").
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

/// Keychain password for a client-side TLS keychain.
///
/// Reads `EPICS_PVA_TLS_KEYCHAIN_PASSWORD`. `$(VAR)` / `${VAR}` refs
/// are expanded. Returns `None` when unset.
pub fn client_tls_keychain_password() -> Option<String> {
    std::env::var("EPICS_PVA_TLS_KEYCHAIN_PASSWORD")
        .ok()
        .map(|s| expand_dollar_vars(&s))
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
            s.split(|c: char| c == ',' || c.is_whitespace())
                .filter_map(|t| t.trim().parse::<IpAddr>().ok())
                .collect()
        })
        .unwrap_or_default()
}

/// Parse `EPICS_PVAS_INTF_ADDR_LIST` — server-side interface bind list.
/// Falls back to `EPICS_PVA_INTF_ADDR_LIST` when unset; if both are
/// empty, returns an empty list (caller should bind 0.0.0.0).
pub fn server_intf_addr_list() -> Vec<IpAddr> {
    if let Ok(s) = std::env::var("EPICS_PVAS_INTF_ADDR_LIST") {
        // PVA-466: expand $(VAR) refs before tokenising.
        let s = expand_dollar_vars(&s);
        return s
            .split(|c: char| c == ',' || c.is_whitespace())
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
pub fn server_ignore_addr_list() -> Vec<(IpAddr, u16)> {
    let Ok(raw) = std::env::var("EPICS_PVAS_IGNORE_ADDR_LIST") else {
        return Vec::new();
    };
    // PVA-466: expand $(VAR) refs before tokenising.
    let raw = expand_dollar_vars(&raw);
    raw.split(|c: char| c == ',' || c.is_whitespace())
        .filter_map(|s| {
            let s = s.trim();
            if s.is_empty() {
                return None;
            }
            if let Ok(sa) = s.parse::<SocketAddr>() {
                return Some((sa.ip(), sa.port()));
            }
            if let Ok(ip) = s.parse::<IpAddr>() {
                return Some((ip, 0));
            }
            None
        })
        .collect()
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
    let src = std::env::var("EPICS_PVAS_BEACON_ADDR_LIST")
        .ok()
        .or_else(|| std::env::var("EPICS_PVA_ADDR_LIST").ok());
    src.map(|s| parse_addr_list_with_port(&s, server_broadcast_port()))
        .unwrap_or_default()
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
    fn parse_bool_yes_y_1_true() {
        assert!(parse_bool("YES"));
        assert!(parse_bool("yes"));
        assert!(parse_bool("Y"));
        assert!(parse_bool("1"));
        assert!(parse_bool("True"));
        assert!(!parse_bool("NO"));
        assert!(!parse_bool("0"));
        assert!(!parse_bool(""));
    }

    #[test]
    fn parse_addr_list_default_port() {
        let addrs = parse_addr_list_with_port("1.2.3.4 5.6.7.8:9876", 1234);
        assert_eq!(addrs.len(), 2);
        assert_eq!(addrs[0].port(), 1234);
        assert_eq!(addrs[1].port(), 9876);
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
}
