//! Environment-variable parsers for `EPICS_PVA_*` / `EPICS_PVAS_*`.
//!
//! Pure functions — they read `std::env::var(...)` directly so the
//! caller doesn't need to thread a Config struct. Where pvxs has
//! Config::fromEnv() that builds an internal config object, we expose
//! one helper per variable. Server-side helpers fall back to the
//! corresponding client-side variable when the `EPICS_PVAS_*` form is
//! not set, matching pvxs's `Config::server()` behavior.

use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::net::{IpAddr, SocketAddr};
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
/// no validation — caller is responsible for using real `EPICS_PVA[S]_*`
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
/// Scoped definitions and the ambient environment are two SEPARATE
/// sources, mirroring pvxs's `PickOne{useenv}` (config.cpp:228-249):
/// [`PvaConfigDefs::get`] reads only this config's definitions
/// (pvxs `useenv=false`), while the ambient process environment is read
/// through this module's `*_from_env` helpers / `std::env::var`
/// (pvxs `useenv=true`). They are deliberately NOT a fallback chain — a
/// missing scoped key must not silently inherit an unrelated process
/// variable, which is the cross-config contamination this primitive
/// exists to prevent. Compose `get` with the pure parsers in this module
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

    /// Resolve `name` against this config's scoped definitions ONLY.
    ///
    /// pvxs `applyDefs(defs)` runs `_fromDefs(..., useenv=false)`, whose
    /// `PickOne` searches only the supplied `defs` map and NEVER calls
    /// `getenv()` (config.cpp:228-249, :468-470, :607-609); an absent key
    /// leaves the config field at its current/default value. So a key this
    /// config does not define returns `None` — it does **not** fall back to
    /// the ambient environment. The earlier fallback re-introduced exactly
    /// the cross-config contamination scoped defs prevent: two configs
    /// could each inherit unrelated process variables (e.g.
    /// `EPICS_PVA_NAME_SERVERS`) through their missing keys. For the pvxs
    /// `fromEnv` source (`useenv=true`), read the process environment
    /// explicitly via this module's `*_from_env` helpers.
    pub fn get(&self, name: &str) -> Option<String> {
        self.defs.get(name).cloned()
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
///
/// Enumerates through [`epics_base_rs::net::iface_v4`] rather than
/// `if-addrs`, so it exists on every target. That is the enumerator the
/// design doc's §8.1 item owed: newlib and VxWorks's `libc` module expose no
/// `getifaddrs`, and `iface_v4` is the workspace's own walk against the BSP's
/// interface table. The client's UDP SEARCH path needs `@iface` to resolve on
/// the embedded targets too, and a name that silently failed to resolve there
/// would send a multicast join out the wrong interface.
pub fn resolve_iface_v4(spec: &str) -> Result<Ipv4Addr, String> {
    if let Ok(v4) = spec.parse::<Ipv4Addr>() {
        return Ok(v4);
    }
    let ifaces = epics_base_rs::net::iface_v4::enumerate()
        .map_err(|e| format!("interface enumeration failed: {e}"))?;
    for iface in ifaces {
        if iface.name == spec {
            return Ok(iface.ip);
        }
    }
    Err(format!("interface {spec:?} has no IPv4 address"))
}

/// Resolve a single address token (no multicast modifiers) to a
/// `SocketAddr`, appending `default_port` when no port is present. Shared
/// by [`Endpoint::parse`] and [`parse_addr_list_with_port`].
///
/// An explicit `:0` port (or a host that resolves to port 0) is normalized
/// to `default_port`, mirroring pvxs `split_addr_into`
/// (`config.cpp:167-168`: `if(ep.addr.port()==0) ep.addr.setPort(defaultPort)`).
/// pvxs applies this uniformly to every env address list — name servers
/// (`tcp_port`), `EPICS_PVA_ADDR_LIST` / beacon destinations (`udp_port`) —
/// because port 0 is never a usable SEARCH/connect destination. When the
/// caller passes `default_port == 0` (e.g. a server bind list that allows an
/// ephemeral port), `setPort(0)` is a no-op, also matching pvxs.
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
    let mut addr = if let Ok(sa) = s.parse::<SocketAddr>() {
        sa
    } else if let Ok(ip) = s.parse::<IpAddr>() {
        SocketAddr::new(ip, default_port)
    } else {
        let with_port = if s.contains(':') {
            s.to_string()
        } else {
            format!("{s}:{default_port}")
        };
        match with_port.to_socket_addrs() {
            Ok(iter) => pick_v4_preferred(iter).or_else(|| {
                tracing::debug!(token = %s, "EPICS_PVA addr-list: empty resolution");
                None
            })?,
            Err(e) => {
                tracing::debug!(token = %s, error = %e, "EPICS_PVA addr-list: resolve failed");
                return None;
            }
        }
    };
    // pvxs `split_addr_into` (config.cpp:167-168) normalizes an explicit
    // `:0` (or a host resolving to port 0) up to the list's default port;
    // `set_port(0)` when `default_port == 0` is a no-op. Without this, an
    // `EPICS_PVA_NAME_SERVERS=host:0` token reached the search engine as a
    // port-0 TCP destination instead of `host:5075`.
    if addr.port() == 0 {
        addr.set_port(default_port);
    }
    Some(addr)
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

/// Collapse duplicate beacon endpoints, mirroring pvxs
/// `removeDups<SockEndpoint>` (`config.cpp:349-371`): duplicates are keyed
/// by `(addr, iface)` and combined into the FIRST occurrence carrying the
/// LONGEST TTL, preserving first-appearance order. pvxs applies this to
/// `beaconDestinations` in `Config::expand()` (`config.cpp:523`), AFTER
/// auto-broadcast / multicast-group expansion, so `removeDups` sees the
/// fully assembled list.
///
/// Without it, `EPICS_PVAS_BEACON_ADDR_LIST="224.0.2.3,1 224.0.2.3,8"`
/// emitted two multicast beacons (TTL 1 and TTL 8) where pvxs emits one at
/// TTL 8, and `@iface` duplicates double-beaconed a NIC. A `None` (OS
/// default) TTL ranks below any explicit TTL via `Option` ordering, so a
/// specified TTL always wins the combine.
pub fn dedup_endpoints(endpoints: Vec<Endpoint>) -> Vec<Endpoint> {
    let mut out: Vec<Endpoint> = Vec::with_capacity(endpoints.len());
    for ep in endpoints {
        if let Some(existing) = out
            .iter_mut()
            .find(|e| e.addr == ep.addr && e.iface == ep.iface)
        {
            // duplicate (addr, iface): keep the longest TTL on the
            // first-seen entry — pvxs `if(ep.ttl > orig.ttl) orig.ttl = ep.ttl`.
            if ep.ttl > existing.ttl {
                existing.ttl = ep.ttl;
            }
        } else {
            out.push(ep);
        }
    }
    out
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
/// The accepted grammar is EXACT to pvxs for env parity: case-insensitive
/// `YES`/`NO` (`epicsStrCaseCmp`) or the literal `1`/`0` (`val=="1"` /
/// `val=="0"`), with NO surrounding-whitespace tolerance. `Y`, `TRUE`,
/// `N`, `FALSE`, and any trimmed value such as `" NO "` are invalid
/// (`None` + a warning), so the caller keeps its default — pvxs treats
/// them the same.
fn parse_bool(name: &str, raw: &str) -> Option<bool> {
    if raw.eq_ignore_ascii_case("YES") || raw == "1" {
        Some(true)
    } else if raw.eq_ignore_ascii_case("NO") || raw == "0" {
        Some(false)
    } else {
        tracing::warn!(
            var = name,
            value = raw,
            "invalid PVA boolean env value (expected YES/NO); keeping default"
        );
        None
    }
}

/// Largest timeout pvxs `parse_timeout` accepts: `double(time_t::max)`
/// (`config.cpp:218`). Anything above it is out-of-range and the
/// destination keeps its default. This also keeps every downstream
/// `Duration::from_secs_f64` below the `u64::MAX` seconds panic edge
/// (~1.845e19 s) even after the 4/3 `tmoScale`.
const TIMEOUT_SECS_MAX: f64 = i64::MAX as f64;

/// pvxs `parseTo<double>` (`util.cpp:769-783`): `std::stod` — which skips
/// leading whitespace and accepts C99 hex-float syntax (`0x1.8p3`) — plus
/// the extraneous-character check, which tolerates trailing whitespace and
/// rejects any other trailing text. Returns `None` where `parseTo` throws
/// `NoConvert`.
fn parse_double_pvxs(raw: &str) -> Option<f64> {
    let s = raw.trim();
    if s.is_empty() {
        return None;
    }
    let (neg, body) = match s.as_bytes()[0] {
        b'-' => (true, &s[1..]),
        b'+' => (false, &s[1..]),
        _ => (false, s),
    };
    let v = if let Some(hex) = body.strip_prefix("0x").or_else(|| body.strip_prefix("0X")) {
        parse_hex_float(hex)?
    } else {
        // Rust's f64 grammar covers the rest of stod's: decimal, exponent,
        // `inf`/`infinity`/`nan` (case-insensitive). Overflowing text such
        // as `1e400` yields `inf` here where stod throws out_of_range; both
        // end at "keep the default" once the finite gate below runs.
        body.parse::<f64>().ok()?
    };
    Some(if neg { -v } else { v })
}

/// The hex-float half of `std::stod`: `<hexdigits>[.<hexdigits>][p[+-]<dec>]`
/// scaled by 2^exp. The binary exponent is optional (`strtod` subject
/// sequence), so `0x10` is 16.0.
fn parse_hex_float(s: &str) -> Option<f64> {
    let (mantissa, exp) = match s.find(['p', 'P']) {
        Some(i) => (&s[..i], Some(&s[i + 1..])),
        None => (s, None),
    };
    let (int_part, frac_part) = match mantissa.find('.') {
        Some(i) => (&mantissa[..i], &mantissa[i + 1..]),
        None => (mantissa, ""),
    };
    if int_part.is_empty() && frac_part.is_empty() {
        return None;
    }
    let mut v = 0.0f64;
    for c in int_part.chars() {
        v = v * 16.0 + f64::from(c.to_digit(16)?);
    }
    let mut scale = 1.0 / 16.0;
    for c in frac_part.chars() {
        v += f64::from(c.to_digit(16)?) * scale;
        scale /= 16.0;
    }
    let e: i32 = match exp {
        Some(e) => e.strip_prefix('+').unwrap_or(e).parse().ok()?,
        None => 0,
    };
    Some(v * 2.0f64.powi(e))
}

/// The single owner of every PVA timeout/period env double, reproducing
/// pvxs `parse_timeout` (`config.cpp:211-227`): parse with `parseTo<double>`
/// (`parse_double_pvxs`), then REJECT a value that is non-finite, negative,
/// or above `double(time_t::max)` — logging and leaving the destination at
/// its default rather than saturating it. (This is deliberately unlike
/// `epics-ca-rs`'s `envGetDoubleConfigParam`, which is epics-base's clamp
/// semantics; pvxs governs here.)
///
/// Before this owner existed each getter did `parse::<f64>()` + a bare
/// `is_finite() && > 0.0` filter and handed the result to
/// `Duration::from_secs_f64`, which **panics** above `u64::MAX` seconds:
/// `EPICS_PVA_CONN_TMO=1e300` passed the filter and aborted every client
/// and server at startup. The range gate here makes that unrepresentable —
/// a value that survives is always convertible to a `Duration`, including
/// after the 4/3 `tmoScale` its owners apply (`TIMEOUT_SECS_MAX`).
///
/// Zero is rejected here rather than passed through as pvxs does, because
/// every caller's default is what pvxs's `enforceTimeout` (`config.cpp:373-391`)
/// / floor rule turns a zero into anyway: `EPICS_PVA_CONN_TMO=0` yields
/// pvxs's 40 s effective idle timeout, and the port's default 30 s × 4/3 is
/// the same 40 s.
pub fn parse_timeout_env(name: &str, raw: &str) -> Option<f64> {
    match parse_double_pvxs(raw) {
        Some(v) if v.is_finite() && v > 0.0 && v <= TIMEOUT_SECS_MAX => Some(v),
        _ => {
            tracing::warn!(
                var = name,
                value = raw,
                "invalid double value; keeping default"
            );
            None
        }
    }
}

/// Parse a PVA port env value the way pvxs does. pvxs `_fromDefs` parses
/// every `EPICS_PVA*_{SERVER,BROADCAST}_PORT` with `parseTo<uint64_t>`
/// (`util.cpp:786-800` — `std::stoull` with leading/trailing whitespace
/// tolerance) and then ASSIGNS the `uint64_t` into the `unsigned short`
/// port field (`server.h:168/170`, `client.h:1030/1033`), truncating to
/// the low 16 bits; a parse error logs and leaves the port at its
/// prior/default value (`config.cpp:402-414, 556-570`).
///
/// So `EPICS_PVAS_SERVER_PORT=70000` becomes TCP port `4464`
/// (`70000 & 0xFFFF`) under pvxs, and `" 5076 "` is accepted. A direct
/// `parse::<u16>()` rejected both (out of range / surrounding whitespace)
/// and fell back to the default, putting a Rust client/server on a
/// different port than a C IOC in the same environment. This helper is
/// the single owner of that rule; every scalar port getter routes
/// through it, then applies its own zero-normalization on the result.
/// Returns the truncated `u16`, or `None` when the value is not a
/// base-10 integer (caller keeps its default).
///
/// pvxs `stoull(.., 0)` additionally accepts `0x`/`0`-prefixed
/// hex/octal and wraps a leading `-`; those pathological config inputs
/// are intentionally not reproduced (decimal only), so this is strictly
/// narrower than pvxs on non-decimal text while matching it on every
/// realistic decimal port value.
pub fn parse_port_env(raw: &str) -> Option<u16> {
    raw.trim().parse::<u64>().ok().map(|v| v as u16)
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
        .and_then(|s| parse_port_env(&s))
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
        .and_then(|s| parse_port_env(&s))
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
/// likewise normalized to this effective port by `resolve_token_addr`
/// (pvxs `split_addr_into` `config.cpp:167-168`), so both the bare token and
/// `host:0` resolve to `5075`.
pub fn server_port() -> u16 {
    let parsed = std::env::var("EPICS_PVA_SERVER_PORT")
        .ok()
        .or_else(|| std::env::var("EPICS_PVAS_SERVER_PORT").ok())
        .and_then(|s| parse_port_env(&s))
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
        .and_then(|s| parse_port_env(&s))
}

/// Presence-aware server **TLS** listen port — `EPICS_PVAS_TLS_PORT`
/// first, then the shared `EPICS_PVA_TLS_PORT`, returning `None` when
/// neither is set so [`crate::server_native::PvaServerConfig::with_env`]
/// preserves a caller-supplied `tls_port`.
///
/// Mirrors pvxs `server::Config::_fromDefs` `PickOne` precedence
/// (`config.cpp:513-519`): the server reads the server-specific
/// `EPICS_PVAS_TLS_PORT` first and falls back to the shared
/// `EPICS_PVA_TLS_PORT`. The compiled default (pvxs `netcommon.h:133`,
/// `tls_port = 5076`) lives on
/// [`crate::server_native::PvaServerConfig::default`], not here, so an
/// absent variable never overwrites the caller value.
pub fn pvas_tls_port_opt() -> Option<u16> {
    std::env::var("EPICS_PVAS_TLS_PORT")
        .ok()
        .or_else(|| std::env::var("EPICS_PVA_TLS_PORT").ok())
        .and_then(|s| parse_port_env(&s))
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
/// burst-interval; see [`crate::server_native::PvaServerConfig`]
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
        // Negatives, zero, NaN, infinity, and out-of-range values are
        // rejected by the shared resolver, so the conversion cannot panic.
        .and_then(|s| parse_timeout_env("EPICS_PVAS_BEACON_PERIOD", &s))
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
        .and_then(|s| parse_timeout_env("EPICS_PVAS_BEACON_PERIOD_LONG", &s))
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
/// [`crate::server_native::PvaServerConfig::max_ops_per_channel`]
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
        .and_then(|s| parse_timeout_env("EPICS_PVA_CONN_TMO", &s))
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
        .and_then(|s| parse_timeout_env("EPICS_PVAS_SEND_TMO", &s))
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
        .and_then(|s| parse_timeout_env("EPICS_PVAS_TLS_HANDSHAKE_TMO", &s))
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

/// Presence-aware `disable_plaintext` flag parsed from the server TLS
/// options (`EPICS_PVAS_TLS_OPTIONS` → `EPICS_PVA_TLS_OPTIONS`, see
/// [`server_tls_options`]).
///
/// Mirrors pvxs `parseTLSOptions` (`config.cpp:435-464`): the option
/// string is whitespace-split into `key=value` tokens (`split_into`,
/// `config.cpp:224-239`) and the flag is set ONLY for the exact value
/// `true` / `false`; an unknown value is ignored and a later token wins
/// over an earlier one. Returns `None` when no `disable_plaintext=` token
/// is present, so [`crate::server_native::PvaServerConfig::with_env`]
/// preserves a caller-supplied value rather than forcing it to the
/// default — the same `PickOne`-style presence rule the port helpers use.
pub fn server_tls_disable_plaintext_opt() -> Option<bool> {
    let mut out = None;
    for tok in server_tls_options().split_ascii_whitespace() {
        match tok.split_once('=') {
            Some(("disable_plaintext", "true")) => out = Some(true),
            Some(("disable_plaintext", "false")) => out = Some(false),
            // Unknown value (or some other key) — pvxs logs + ignores,
            // leaving the field unchanged.
            _ => {}
        }
    }
    out
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

/// Resolve a `EPICS_PVA*_INTF_ADDR_LIST` value to a deduplicated list of
/// bind `IpAddr`s.
///
/// pvxs parses interface lists through the SAME `split_addr_into` /
/// `SockEndpoint` path as every other address list (config.cpp:151-169,
/// 418-419, 592-593), so a DNS hostname is resolved before it constrains
/// the bind/search interfaces: `SockEndpoint` calls `SockAddr::setAddress`
/// (util.cpp:523-540), which falls back to a synchronous DNS lookup,
/// IPv4-preferred. The earlier `parse::<IpAddr>()`-only split dropped any
/// hostname token, silently turning a constrained interface list into the
/// empty all-NIC default (client search/broadcast) or wildcard bind
/// (server listener) — a broader exposure than pvxs.
///
/// Routes each whitespace-separated token (pvxs `split_addr_into` splits on
/// whitespace; the comma is endpoint syntax, never a list separator)
/// through the shared [`resolve_token_addr`] resolver — literal IP /
/// `ip:port` / hostname / `host:port`, IPv4-preferred DNS — and keeps the
/// resolved `IpAddr`; the port is irrelevant for a bind interface, so the
/// default port is `0`. Unresolvable tokens are dropped with a `warn` so an
/// operator sees a mistyped interface. Duplicates are collapsed after
/// resolution, preserving first-appearance order (pvxs normalizes then
/// `removeDups`). One owner for both the client and server interface lists
/// so the family cannot drift back to dropping hostnames at one site.
fn resolve_intf_addr_list(value: &str) -> Vec<IpAddr> {
    // PVA-466: expand $(VAR) / ${VAR} refs before tokenising.
    let value = expand_dollar_vars(value);
    let mut out: Vec<IpAddr> = Vec::new();
    for token in value.split_whitespace() {
        match resolve_token_addr(token, 0) {
            Some(sa) => {
                let ip = sa.ip();
                if !out.contains(&ip) {
                    out.push(ip);
                }
            }
            None => tracing::warn!(
                token = %token,
                "EPICS_PVA*_INTF_ADDR_LIST: unresolvable interface, skipping"
            ),
        }
    }
    out
}

/// Parse `EPICS_PVA_INTF_ADDR_LIST` — client-side interface bind list.
/// Empty = bind to 0.0.0.0 (default behaviour). DNS hostnames resolve
/// (IPv4-preferred) and `$(VAR)` / `${VAR}` refs expand; see
/// `resolve_intf_addr_list`.
pub fn list_intf_addresses() -> Vec<IpAddr> {
    std::env::var("EPICS_PVA_INTF_ADDR_LIST")
        .ok()
        .map(|s| resolve_intf_addr_list(&s))
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
        // Same hostname-resolving path as the client list — see
        // [`resolve_intf_addr_list`]. A DNS NIC name (`ioc-public-nic`)
        // must resolve to its IPv4 address, not be dropped, so the server
        // binds the named interface instead of falling back to wildcard.
        return resolve_intf_addr_list(&s);
    }
    list_intf_addresses()
}

/// PVX-82: presence-and-validity-aware server INTF resolver. pvxs parses
/// `EPICS_PVAS_INTF_ADDR_LIST` with `required=true` (`config.cpp:418-424`
/// → `151-176`, throwing at `172-174`), so a malformed endpoint aborts
/// server config. The Rust resolver is intentionally lenient — it warns
/// and drops an unresolvable token, which is fine for a *partially* valid
/// list — but a list whose tokens **all** drop must NOT silently become an
/// empty list, because the bind path ([`super::super::server_native`]'s
/// `tcp_bind_addresses`) then promotes empty to the wildcard `0.0.0.0`,
/// turning a typo'd bind-restriction into a listen-on-every-interface.
/// Distinguish the three cases so the server can refuse that silent
/// over-broad bind without failing on every benign typo in a partly-valid
/// list:
///
/// - `Ok(None)` — neither var set, or the value is whitespace-only ⟹
///   the operator wants the wildcard ("no addresses isn't interesting",
///   `config.cpp:492-494`).
/// - `Ok(Some(addrs))` — at least one token resolved.
/// - `Err(msg)` — non-blank token(s) present but **none** resolved ⟹ the
///   requested restriction is empty; fail loudly instead of binding all
///   interfaces.
pub fn server_intf_addr_list_checked() -> Result<Option<Vec<IpAddr>>, String> {
    let raw = std::env::var("EPICS_PVAS_INTF_ADDR_LIST")
        .ok()
        .or_else(|| std::env::var("EPICS_PVA_INTF_ADDR_LIST").ok());
    let Some(raw) = raw else {
        return Ok(None);
    };
    // `resolve_intf_addr_list` expands `$(VAR)` internally; expand here too
    // so the "did the operator name any interface" test sees the same text
    // (an env-ref that expands to nothing counts as unset → wildcard).
    let had_token = expand_dollar_vars(&raw).split_whitespace().next().is_some();
    if !had_token {
        return Ok(None);
    }
    let addrs = resolve_intf_addr_list(&raw);
    if addrs.is_empty() {
        Err(format!(
            "EPICS_PVA[S]_INTF_ADDR_LIST=\"{raw}\" named interface(s) but none \
             resolved; refusing to silently bind the wildcard 0.0.0.0 (every \
             interface). Fix the interface list, or unset it to bind all \
             interfaces intentionally."
        ))
    } else {
        Ok(Some(addrs))
    }
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

/// PVX-82 (IGNORE sibling of [`server_intf_addr_list_checked`]):
/// presence-and-validity-aware server blocklist resolver. pvxs parses
/// `EPICS_PVAS_IGNORE_ADDR_LIST` with `required=true` (`config.cpp:422-423`
/// → `151-176`, throwing at `172-174`) exactly as it does the INTF list, so
/// a malformed token aborts server config. Mirror the INTF treatment: a
/// non-blank list whose tokens **all** fail to resolve is a misconfiguration
/// — the operator named peers to block but none were understood, so the
/// blocklist the operator asked for would silently be empty. Fail loudly
/// instead of dropping every entry.
///
/// - `Ok(None)` — unset or whitespace-only ⟹ no blocklist (preserve a
///   caller-supplied `ignore_addrs`).
/// - `Ok(Some(entries))` — at least one token resolved.
/// - `Err(msg)` — non-blank token(s) present but **none** resolved.
///
/// Like INTF this is the **all**-bad gate (not pvxs's per-token **any**-bad
/// throw); the partial-list leniency is the documented shared residual.
pub fn server_ignore_addr_list_checked() -> Result<Option<Vec<(IpAddr, u16)>>, String> {
    let Ok(raw) = std::env::var("EPICS_PVAS_IGNORE_ADDR_LIST") else {
        return Ok(None);
    };
    let had_token = expand_dollar_vars(&raw).split_whitespace().next().is_some();
    if !had_token {
        return Ok(None);
    }
    let entries = server_ignore_addr_list();
    if entries.is_empty() {
        Err(format!(
            "EPICS_PVAS_IGNORE_ADDR_LIST=\"{raw}\" named peer(s) to block but \
             none resolved; refusing to start with the requested blocklist \
             silently empty. Fix the ignore list, or unset it to disable peer \
             filtering."
        ))
    } else {
        Ok(Some(entries))
    }
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
///
/// Enumerates through [`epics_base_rs::net::iface_v4`], so it exists on every
/// target — see [`resolve_iface_v4`]. The eligibility and destination rules
/// are `IfaceV4::search_destination`'s, which are C's
/// (`osdNetIfAddrs.c:130-151`): up and non-loopback, the broadcast address
/// unless it is `0.0.0.0`, else a point-to-point peer. The `if-addrs` version
/// this replaces tested neither `IFF_UP` nor the point-to-point case, so a
/// configured-but-down NIC contributed a destination and a VPN tunnel
/// contributed none.
pub fn list_broadcast_addresses(port: u16) -> Vec<SocketAddr> {
    let mut out: Vec<SocketAddr> = epics_base_rs::net::iface_v4::broadcast_addrs()
        .into_iter()
        .map(|ip| SocketAddr::new(IpAddr::V4(ip), port))
        .collect();
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
///
/// Enumerates through [`epics_base_rs::net::iface_v4`], so it exists on every
/// target — see [`resolve_iface_v4`].
pub fn list_broadcast_addresses_on(interfaces: &[Ipv4Addr], port: u16) -> Vec<SocketAddr> {
    if interfaces.is_empty() || interfaces.iter().any(|ip| ip.is_unspecified()) {
        return list_broadcast_addresses(port);
    }
    let mut out = Vec::new();
    let mut any_non_loopback = false;
    if let Ok(ifaces) = epics_base_rs::net::iface_v4::enumerate() {
        for want in interfaces {
            if want.is_loopback() {
                continue;
            }
            for iface in &ifaces {
                if &iface.ip == want {
                    any_non_loopback = true;
                    if let Some(dest) = iface.search_destination() {
                        out.push(SocketAddr::new(IpAddr::V4(dest), port));
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

/// pvxs idle-timeout scale (`config.cpp:149` `tmoScale = 4.0/3.0`): a
/// configured `EPICS_PVA_CONN_TMO` of 30 s yields a 40 s effective
/// inactivity timeout, so the server's reap window does not race a Java
/// client's 30 s echo cadence. The raw env parser [`conn_timeout_secs`]
/// keeps the *configured* seconds; `Config` is the effective-timeout
/// owner that applies this scale (`parse_timeout`, `config.cpp:211-227`).
const TMO_SCALE: f64 = 4.0 / 3.0;

/// Clamp an effective (already `4/3`-scaled) TCP idle timeout to the pvxs
/// bounds, mirroring `enforceTimeout` (`config.cpp:373-391`). A
/// non-finite, non-positive, or `>= time_t::max` value resets to the 40 s
/// default; anything under the 2 s floor is raised to 2 s, so a
/// misconfigured timeout can never fall below the echo cadence.
fn enforce_timeout(tmo: &mut f64) {
    if !tmo.is_finite() || *tmo <= 0.0 || *tmo >= i64::MAX as f64 {
        *tmo = 40.0;
    } else if *tmo < 2.0 {
        *tmo = 2.0;
    }
}

/// The ECHO cadence for an *effective* (already scaled + `enforceTimeout`d)
/// TCP timeout: pvxs `max(1.0, min(15.0, tcpTimeout * 3.0/8.0))`
/// (`clientconn.cpp:163`) — "tcpTimeout(40) -> 15 second echo period,
/// bound echo to range [1, 15]".
///
/// SINGLE OWNER of "effective TCP timeout → echo period". Both the
/// env-derived `client_native::server_conn::heartbeat_interval` and the
/// per-connection heartbeat task (which uses the builder-supplied
/// `tcp_timeout`) derive their cadence here, so a connection cannot echo on
/// a different clock than the API says it does.
///
/// The 15 s CAP is the half C keeps and the port dropped: without it a
/// large CONN_TMO stretched the echo period without bound (a 100 s CONN_TMO
/// echoed every 50 s instead of C's 15 s), so a peer that had already
/// stopped responding was probed far later than pvxs probes it (R17-36).
///
/// The input is always a finite, positive effective timeout — it comes from
/// [`effective_tcp_timeout_secs`] (which maps NaN / non-positive to 40 s) or
/// from a `Duration` — so `clamp`'s NaN caveat cannot be reached here.
pub fn echo_period_secs(tcp_timeout_secs: f64) -> f64 {
    (tcp_timeout_secs * 3.0 / 8.0).clamp(1.0, 15.0)
}

/// The EFFECTIVE TCP inactivity timeout for a *configured*
/// `EPICS_PVA_CONN_TMO` value: the `tmoScale` (4/3) followed by pvxs
/// `enforceTimeout` (`config.cpp:373-391`), which C applies to the SCALED
/// `tcpTimeout` (`config.cpp:527`/`:650`, after `parse_timeout` already
/// multiplied by the scale).
///
/// SINGLE OWNER of "configured CONN_TMO → effective idle timeout". Both
/// ends derive their window here — the native server's `idle_timeout` and
/// the client's `heartbeat_timeout` — so neither can reproduce half the
/// rule. The rule has BOTH bounds: `>= double(time_t::max)` (or
/// non-finite / non-positive) resets to 40 s, and only then is the 2 s
/// floor applied. Reproducing the floor alone left `CONN_TMO` in
/// `[6.92e18, 9.22e18]` — values `parse_timeout` accepts, whose scaled
/// form crosses `time_t::max` — running with a ~1.2e19 s window where
/// pvxs falls back to 40 s (R17-34).
pub fn effective_tcp_timeout_secs(configured: f64) -> f64 {
    let mut tmo = configured * TMO_SCALE;
    enforce_timeout(&mut tmo);
    tmo
}

/// Wrap interface IPs as modifier-less [`Endpoint`]s (port 0 — a bind
/// interface carries no destination port). Bridges the `Vec<IpAddr>`
/// interface parsers to `Config`'s endpoint-typed `interfaces` field.
///
/// This and the next two helpers exist only for [`Config::expand`], so
/// they carry the same host-only gate it does.
#[cfg(not(epics_embedded_target))]
fn intf_endpoints(ips: Vec<IpAddr>) -> Vec<Endpoint> {
    ips.into_iter()
        .map(|ip| Endpoint::from(SocketAddr::new(ip, 0)))
        .collect()
}

/// Give every port-less destination the list's default port, mirroring
/// pvxs `split_addr_into` (`config.cpp:167-168`:
/// `if(ep.addr.port()==0) ep.addr.setPort(defaultPort)`). Applied inside
/// [`Config::expand`] so `addr` and `addr:udp_port` do not survive dedup
/// as two distinct effective targets.
#[cfg(not(epics_embedded_target))]
fn set_default_port(eps: &mut [Endpoint], port: u16) {
    for e in eps.iter_mut() {
        if e.addr.port() == 0 {
            e.addr.set_port(port);
        }
    }
}

/// Drop duplicate `(ip, port)` ignore entries, preserving first-seen
/// order (pvxs `removeDups(ignoreAddrs)`, `config.cpp:525`).
#[cfg(not(epics_embedded_target))]
fn dedup_ignore_addrs(addrs: &mut Vec<(IpAddr, u16)>) {
    let mut seen = std::collections::HashSet::new();
    addrs.retain(|x| seen.insert(*x));
}

/// The effective PVA configuration value — a client+server union of
/// `pvxs::client::Config` and `pvxs::server::Config` (`config.cpp`),
/// seeded from the environment via [`Config::from_client_env`] /
/// [`Config::from_server_env`] and finalised by [`Config::expand`].
///
/// `expand()` is the single owner that turns the *as-configured* value
/// into the *as-used* value: it normalises the protocol ports, fills the
/// wildcard interface, fans the auto-address / auto-beacon broadcasts
/// into the destination lists, gives each destination its effective
/// port, collapses duplicates, and clamps the idle timeout — mirroring
/// pvxs `Config::expand()` (`config.cpp:485-529` server, `:624-651`
/// client). The post-`expand()` value is what an effective-config
/// readback (`pvinfo -D`, `operator<<`) should render.
///
/// **Host-only, and the one §8.1 blocker in this crate that a target gate
/// does not actually solve.** [`Config::expand`] fans the auto-address /
/// auto-beacon broadcasts out per NIC, which needs live interface
/// enumeration — `if-addrs`, which neither newlib (RTEMS) nor VxWorks's
/// `libc` module can build. Skipping the fan-out on an embedded target
/// would leave a server that silently never beacons to its subnet, so the
/// type is absent there rather than quietly wrong. Its only users today are
/// the host-only async I/O layer and the `pv*` CLI verbose readback, so
/// nothing on RTEMS or VxWorks is missing a facility it could otherwise
/// use. Closing this for real means an embedded interface enumerator
/// (libbsd `getifaddrs` via a `-sys` binding, or an ioctl walk against BSP
/// headers, on whichever target grows one) behind
/// [`list_broadcast_addresses_on`] — owed with the blocking PVA driver,
/// design doc §8.1 / §9 phase 6 item 7.
#[cfg(not(epics_embedded_target))]
#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    /// `EPICS_PVA_BROADCAST_PORT` — UDP SEARCH / beacon port (pvxs
    /// `udp_port`); the default port for UDP address-list and beacon
    /// destinations.
    pub udp_port: u16,
    /// `EPICS_PVA_SERVER_PORT` — TCP server / name-server port (pvxs
    /// `tcp_port`).
    pub tcp_port: u16,
    /// `EPICS_PVA_AUTO_ADDR_LIST` — when set, `expand()` appends each
    /// interface's directed broadcast to [`Self::address_list`] and then
    /// clears the flag (pvxs `autoAddrList`, `config.cpp:640-643`).
    pub auto_addr_list: bool,
    /// `EPICS_PVAS_AUTO_BEACON_ADDR_LIST` — server analogue of
    /// `auto_addr_list` for [`Self::beacon_destinations`] (pvxs
    /// `auto_beacon`, `config.cpp:512-519`).
    pub auto_beacon: bool,
    /// `EPICS_PVA[S]_INTF_ADDR_LIST` — interface bind / broadcast-source
    /// list. Empty means the wildcard, which `expand()` fills with
    /// `0.0.0.0` (`config.cpp:492-494, 637-638`).
    pub interfaces: Vec<Endpoint>,
    /// `EPICS_PVA_ADDR_LIST` — UDP SEARCH targets. pva2pva treats these,
    /// together with [`Self::udp_port`], as UDP SEARCH destinations,
    /// never TCP name servers.
    pub address_list: Vec<Endpoint>,
    /// `EPICS_PVA_NAME_SERVERS` — persistent TCP SEARCH peers (distinct
    /// from [`Self::address_list`]).
    pub name_servers: Vec<SocketAddr>,
    /// `EPICS_PVAS_BEACON_ADDR_LIST` — explicit beacon destinations.
    pub beacon_destinations: Vec<Endpoint>,
    /// `EPICS_PVAS_IGNORE_ADDR_LIST` — peers to drop (`port == 0` matches
    /// any port from that IP).
    pub ignore_addrs: Vec<(IpAddr, u16)>,
    /// Effective (`4/3`-scaled) TCP idle timeout, seconds. `expand()`
    /// clamps it to the pvxs bounds via `enforce_timeout`.
    pub tcp_timeout: f64,
}

#[cfg(not(epics_embedded_target))]
impl Config {
    /// Seed a client-role config from the `EPICS_PVA_*` environment.
    /// Mirrors `client::Config::fromEnv` (`config.cpp:552-599`): each
    /// field comes from the matching env accessor, the address list is
    /// parsed against the UDP port, and the timeout is scaled. Call
    /// [`Self::expand`] before use.
    pub fn from_client_env() -> Self {
        let udp_port = broadcast_port();
        let address_list = std::env::var("EPICS_PVA_ADDR_LIST")
            .ok()
            .map(|s| parse_endpoints_with_port(&s, udp_port))
            .unwrap_or_default();
        Self {
            udp_port,
            tcp_port: server_port(),
            auto_addr_list: auto_addr_list_enabled(),
            auto_beacon: false,
            interfaces: intf_endpoints(list_intf_addresses()),
            address_list,
            name_servers: name_servers(),
            beacon_destinations: Vec::new(),
            ignore_addrs: Vec::new(),
            tcp_timeout: conn_timeout_secs() * TMO_SCALE,
        }
    }

    /// Seed a server-role config from the `EPICS_PVAS_*` environment
    /// (falling back to `EPICS_PVA_*`). Mirrors `server::Config::fromEnv`
    /// (`config.cpp:397-445`). Call [`Self::expand`] before use.
    pub fn from_server_env() -> Self {
        Self {
            udp_port: server_broadcast_port(),
            tcp_port: pvas_server_port(),
            auto_addr_list: false,
            auto_beacon: auto_beacon_addr_list_enabled(),
            interfaces: intf_endpoints(server_intf_addr_list()),
            address_list: Vec::new(),
            name_servers: Vec::new(),
            beacon_destinations: server_beacon_endpoints_opt().unwrap_or_default(),
            ignore_addrs: server_ignore_addr_list(),
            tcp_timeout: conn_timeout_secs() * TMO_SCALE,
        }
    }

    /// Finalise the configuration in place — see the type docs. Idempotent:
    /// the `auto_*` flags are cleared once consumed, so a second call is a
    /// no-op fan-out (pvxs sets `autoAddrList = false` / `auto_beacon =
    /// false` for the same reason, `config.cpp:643, 518`).
    pub fn expand(&mut self) {
        // A zero effective port is never a usable destination, so promote
        // it to the protocol default (config.cpp:563-566, 575-578,
        // 628-632). The server bind port keeps its own zero-means-ephemeral
        // semantics elsewhere; this is the *effective destination* port.
        if self.udp_port == 0 {
            self.udp_port = 5076;
        }
        if self.tcp_port == 0 {
            self.tcp_port = 5075;
        }

        // An empty interface list implies the wildcard — "no addresses
        // isn't interesting" (config.cpp:492-494, 637-638).
        if self.interfaces.is_empty() {
            self.interfaces.push(Endpoint::from(SocketAddr::new(
                IpAddr::V4(Ipv4Addr::UNSPECIFIED),
                0,
            )));
        }

        let v4_ifaces = self.interface_v4_addrs();

        // Auto address-list expansion (client, config.cpp:640-643): append
        // each interface's directed broadcast to the SEARCH targets, then
        // clear the flag so a re-`expand()` does not duplicate them.
        if self.auto_addr_list {
            for sa in list_broadcast_addresses_on(&v4_ifaces, self.udp_port) {
                self.address_list.push(Endpoint::from(sa));
            }
            self.auto_addr_list = false;
        }

        // Auto beacon expansion (server, config.cpp:512-519): the same
        // broadcast fan-out into the beacon destinations.
        if self.auto_beacon {
            for sa in list_broadcast_addresses_on(&v4_ifaces, self.udp_port) {
                self.beacon_destinations.push(Endpoint::from(sa));
            }
            self.auto_beacon = false;
        }

        // Give every port-less destination its effective UDP port before
        // dedup, so `addr` and `addr:udp_port` collapse together.
        set_default_port(&mut self.address_list, self.udp_port);
        set_default_port(&mut self.beacon_destinations, self.udp_port);

        // Collapse duplicates — longest TTL wins, first-seen order
        // (config.cpp:521-525, 647 `removeDups`).
        self.interfaces = dedup_endpoints(std::mem::take(&mut self.interfaces));
        self.address_list = dedup_endpoints(std::mem::take(&mut self.address_list));
        self.beacon_destinations = dedup_endpoints(std::mem::take(&mut self.beacon_destinations));
        dedup_ignore_addrs(&mut self.ignore_addrs);

        // Clamp the idle timeout to the pvxs bounds (config.cpp:527, 650).
        enforce_timeout(&mut self.tcp_timeout);
    }

    /// The IPv4 addresses of [`Self::interfaces`], used as the interface
    /// constraint for broadcast auto-expansion. The wildcard `0.0.0.0`
    /// entry maps to itself, so [`list_broadcast_addresses_on`] expands it
    /// to every NIC.
    fn interface_v4_addrs(&self) -> Vec<Ipv4Addr> {
        self.interfaces
            .iter()
            .filter_map(|e| match e.addr.ip() {
                IpAddr::V4(v4) => Some(v4),
                IpAddr::V6(_) => None,
            })
            .collect()
    }
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

        // A key this config does not define resolves to None — pvxs
        // `useenv=false` reads only the defs map, never the env.
        assert!(cfg_a.get(UNSET).is_none(), "undefined key resolves to None");
        assert!(!cfg_a.contains(UNSET));
    }

    /// pvxs `applyDefs(defs)` -> `_fromDefs(useenv=false)`: `PickOne`
    /// searches only the defs map and never calls `getenv()`
    /// (config.cpp:228-249, :468-470). A key present in the process
    /// environment but absent from the scoped defs must NOT leak into the
    /// scoped config — that ambient fallback was the contamination this
    /// primitive exists to prevent.
    #[test]
    #[serial_test::serial(epics_env)]
    fn pva_config_defs_get_does_not_fall_back_to_ambient_env() {
        const KEY: &str = "EPICS_PVA_NAME_SERVERS";
        unsafe {
            std::env::set_var(KEY, "10.9.9.9:5075");
        }
        // Scoped defs deliberately omit KEY.
        let scoped = PvaConfigDefs::apply_defs(&HashMap::new());
        let scoped_got = scoped.get(KEY);
        // The ambient-env source (pvxs `useenv=true`) still holds the value
        // — the two sources are distinct, not chained.
        let env_seen = std::env::var(KEY).ok();
        unsafe {
            std::env::remove_var(KEY);
        }
        assert!(
            scoped_got.is_none(),
            "scoped get() must not fall back to ambient env (pvxs useenv=false)"
        );
        assert_eq!(
            env_seen.as_deref(),
            Some("10.9.9.9:5075"),
            "the ambient process-env source still resolves the value independently"
        );
    }

    /// A key absent from one config's defs must not bleed in from another
    /// config's map nor from the process environment.
    #[test]
    #[serial_test::serial(epics_env)]
    fn pva_config_defs_absent_key_does_not_bleed_between_maps_or_env() {
        const KEY: &str = "EPICS_PVA_ADDR_LIST";
        unsafe {
            std::env::set_var(KEY, "172.16.0.1");
        }
        let mut a = HashMap::new();
        a.insert(KEY.to_string(), "10.0.0.1".to_string());
        let cfg_a = PvaConfigDefs::apply_defs(&a);
        // Map B omits KEY.
        let cfg_b = PvaConfigDefs::apply_defs(&HashMap::new());
        let a_got = cfg_a.get(KEY);
        let b_got = cfg_b.get(KEY);
        unsafe {
            std::env::remove_var(KEY);
        }
        assert_eq!(
            a_got.as_deref(),
            Some("10.0.0.1"),
            "map A keeps its own scoped definition"
        );
        assert!(
            b_got.is_none(),
            "map B must not inherit KEY from map A or from the process env"
        );
    }

    /// The env bool grammar
    /// must be EXACT to pvxs `parse_bool` (config.cpp:199-208). Tested by
    /// the accept/reject boundary, not a narrative: case-insensitive
    /// `YES`/`NO` and literal `1`/`0` accept; `Y`, `TRUE`, `N`, `FALSE`,
    /// and any whitespace-padded value reject (→ `None` → default kept).
    #[test]
    fn parse_bool_exact_to_pvxs_yes_no_one_zero_only() {
        const N: &str = "EPICS_PVA_AUTO_ADDR_LIST";
        // pvxs true set: case-insensitive YES, literal 1.
        assert_eq!(parse_bool(N, "YES"), Some(true));
        assert_eq!(parse_bool(N, "yes"), Some(true));
        assert_eq!(parse_bool(N, "Yes"), Some(true));
        assert_eq!(parse_bool(N, "1"), Some(true));
        // pvxs false set: case-insensitive NO, literal 0.
        assert_eq!(parse_bool(N, "NO"), Some(false));
        assert_eq!(parse_bool(N, "no"), Some(false));
        assert_eq!(parse_bool(N, "0"), Some(false));
        // Non-pvxs extensions are now invalid → None (keep default).
        assert_eq!(parse_bool(N, "Y"), None);
        assert_eq!(parse_bool(N, "TRUE"), None);
        assert_eq!(parse_bool(N, "true"), None);
        assert_eq!(parse_bool(N, "N"), None);
        assert_eq!(parse_bool(N, "FALSE"), None);
        assert_eq!(parse_bool(N, "false"), None);
        // No trimming: a padded YES/NO is invalid, exactly as pvxs's
        // `epicsStrCaseCmp(" NO ", "NO") != 0`.
        assert_eq!(parse_bool(N, " NO "), None);
        assert_eq!(parse_bool(N, " YES"), None);
        assert_eq!(parse_bool(N, "1 "), None);
        // Other invalid values → None so the caller preserves its default.
        assert_eq!(parse_bool(N, "maybe"), None);
        assert_eq!(parse_bool(N, ""), None);
    }

    /// Caller side: a
    /// non-pvxs bool value on a default-enabled var must leave the default
    /// ON, matching pvxs leaving `autoAddrList` untouched on an invalid
    /// value — `N`, `FALSE`, and `" NO "` no longer disable discovery.
    #[test]
    #[serial_test::serial(epics_env)]
    fn auto_addr_list_non_pvxs_bool_preserves_default_enabled() {
        // SAFETY: std::env mutation is unsafe in edition 2024; the
        // `epics_env` serial guard makes it race-free under `cargo test`,
        // which runs all lib tests as threads in one process (unlike
        // nextest, which isolates each test in its own process).
        for invalid in ["N", "FALSE", " NO ", "false"] {
            unsafe { std::env::set_var("EPICS_PVA_AUTO_ADDR_LIST", invalid) };
            assert!(
                auto_addr_list_enabled(),
                "non-pvxs bool {invalid:?} must preserve default true (pvxs keeps auto-addr ON)"
            );
        }
        // The exact pvxs false token still disables.
        unsafe { std::env::set_var("EPICS_PVA_AUTO_ADDR_LIST", "NO") };
        assert!(!auto_addr_list_enabled(), "exact NO disables");
        unsafe { std::env::remove_var("EPICS_PVA_AUTO_ADDR_LIST") };
    }

    #[test]
    #[serial_test::serial(epics_env)]
    fn auto_addr_list_invalid_value_preserves_default_enabled() {
        // pvxs: a misspelled EPICS_PVA_AUTO_ADDR_LIST keeps auto-addr ON;
        // the old truthy-collapse turned it OFF on any non-true string.
        // SAFETY: std::env mutation is unsafe in edition 2024; the
        // `epics_env` serial guard makes it race-free under `cargo test`,
        // which runs all lib tests as threads in one process.
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
    #[serial_test::serial(epics_env)]
    fn broadcast_port_zero_falls_back_to_5076_client_only() {
        // pvxs ignores EPICS_PVA_BROADCAST_PORT=0 and restores 5076 for the
        // client (a SEARCH to UDP port 0 can never reach a server).
        // SAFETY: std::env mutation is unsafe in edition 2024; the
        // `epics_env` serial guard makes it race-free under `cargo test`,
        // which runs all lib tests as threads in one process.
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
    fn parse_port_env_matches_pvxs_uint64_truncation() {
        // pvxs parseTo<uint64_t> then assigns to `unsigned short`:
        // EPICS_PVAS_SERVER_PORT=70000 -> 70000 & 0xFFFF = 4464.
        assert_eq!(parse_port_env("70000"), Some(4464));
        // 65536 truncates to 0 (caller then applies its own zero rule).
        assert_eq!(parse_port_env("65536"), Some(0));
        // leading/trailing whitespace accepted (stoull skips it).
        assert_eq!(parse_port_env(" 5076 "), Some(5076));
        assert_eq!(parse_port_env("\t5075\n"), Some(5075));
        // ordinary in-range values pass through unchanged.
        assert_eq!(parse_port_env("5075"), Some(5075));
        assert_eq!(parse_port_env("0"), Some(0));
        // non-integer / extraneous trailing chars / internal space / empty
        // -> None, so the caller keeps its default (pvxs logs + keeps default).
        assert_eq!(parse_port_env("garbage"), None);
        assert_eq!(parse_port_env("5076x"), None);
        assert_eq!(parse_port_env("50 76"), None);
        assert_eq!(parse_port_env(""), None);
        // beyond u64 range -> None (pvxs throws out_of_range -> default kept).
        assert_eq!(parse_port_env("999999999999999999999999999"), None);
    }

    #[test]
    #[serial_test::serial(epics_env)]
    fn port_getters_truncate_out_of_range_like_pvxs() {
        // The finding's named cases, exercised through the getters.
        // SAFETY: std::env mutation is unsafe in edition 2024; the
        // `epics_env` serial guard makes it race-free under `cargo test`,
        // which runs all lib tests as threads in one process.
        // EPICS_PVAS_SERVER_PORT=70000 -> pvxs TCP port 4464, not the
        // default 5075 a strict u16 parse fell back to.
        unsafe { std::env::set_var("EPICS_PVAS_SERVER_PORT", "70000") };
        assert_eq!(
            pvas_server_port(),
            4464,
            "server bind port truncates 70000 -> 4464"
        );
        unsafe { std::env::remove_var("EPICS_PVAS_SERVER_PORT") };

        // EPICS_PVA_BROADCAST_PORT=70000 -> 4464 (nonzero, no fallback).
        unsafe { std::env::set_var("EPICS_PVA_BROADCAST_PORT", "70000") };
        assert_eq!(
            broadcast_port(),
            4464,
            "client broadcast port truncates 70000 -> 4464"
        );
        // A whitespace-wrapped valid port is accepted (was rejected before).
        unsafe { std::env::set_var("EPICS_PVA_BROADCAST_PORT", " 5099 ") };
        assert_eq!(
            broadcast_port(),
            5099,
            "whitespace-wrapped port accepted -> 5099"
        );
        unsafe { std::env::remove_var("EPICS_PVA_BROADCAST_PORT") };
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
    fn dedup_beacon_endpoints_combines_longest_ttl() {
        // pvxs `removeDups<SockEndpoint>` collapses same-(addr,iface)
        // duplicates into the first occurrence carrying the longest TTL.
        let eps = parse_endpoints_with_port("224.0.2.3,1 224.0.2.3,8", 5076);
        assert_eq!(eps.len(), 2, "two raw tokens before dedup");
        let deduped = dedup_endpoints(eps);
        assert_eq!(deduped.len(), 1, "same (addr,iface) collapses to one");
        assert_eq!(deduped[0].addr, "224.0.2.3:5076".parse().unwrap());
        assert_eq!(deduped[0].ttl, Some(8), "longest TTL survives the combine");
    }

    #[test]
    fn dedup_beacon_endpoints_explicit_ttl_beats_default() {
        // `None` (OS default) TTL ranks below any explicit TTL via `Option`
        // ordering — a specified TTL always wins regardless of token order.
        let default_first =
            dedup_endpoints(parse_endpoints_with_port("224.0.2.3 224.0.2.3,4", 5076));
        assert_eq!(default_first.len(), 1);
        assert_eq!(default_first[0].ttl, Some(4));
        let explicit_first =
            dedup_endpoints(parse_endpoints_with_port("224.0.2.3,4 224.0.2.3", 5076));
        assert_eq!(explicit_first.len(), 1);
        assert_eq!(explicit_first[0].ttl, Some(4));
    }

    #[test]
    fn dedup_beacon_endpoints_keeps_distinct_iface() {
        // Same multicast group on two different interfaces is two distinct
        // destinations (pvxs keys dedup by `(addr, iface)`, not addr alone).
        let eps = parse_endpoints_with_port("224.0.2.3,8@127.0.0.1 224.0.2.3,8@127.0.0.2", 5076);
        let deduped = dedup_endpoints(eps);
        assert_eq!(deduped.len(), 2, "distinct @iface stays separate");
        assert_eq!(deduped[0].iface.as_deref(), Some("127.0.0.1"));
        assert_eq!(deduped[1].iface.as_deref(), Some("127.0.0.2"));
    }

    #[test]
    fn dedup_beacon_endpoints_preserves_first_seen_order() {
        // Non-duplicate endpoints keep first-appearance order; the surviving
        // duplicate stays in its first-seen slot, not the later one.
        let eps = parse_endpoints_with_port("10.0.0.1 224.0.2.3,1 10.0.0.2 224.0.2.3,8", 5076);
        let deduped = dedup_endpoints(eps);
        assert_eq!(deduped.len(), 3, "one (224.0.2.3) duplicate collapsed");
        assert_eq!(deduped[0].addr, "10.0.0.1:5076".parse().unwrap());
        assert_eq!(deduped[1].addr, "224.0.2.3:5076".parse().unwrap());
        assert_eq!(deduped[1].ttl, Some(8), "merged TTL on the first-seen slot");
        assert_eq!(deduped[2].addr, "10.0.0.2:5076".parse().unwrap());
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
    #[serial_test::serial(epics_env)]
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

    /// A DNS hostname in `EPICS_PVA_INTF_ADDR_LIST` /
    /// `EPICS_PVAS_INTF_ADDR_LIST` must resolve to its (IPv4-preferred)
    /// address, matching pvxs `SockAddr::setAddress` (util.cpp:523-540),
    /// not be dropped to an empty list. Pre-fix the parser required a
    /// literal IP, so `localhost` produced `[]` — the all-NIC / wildcard
    /// default — broadening the bind/search surface beyond pvxs.
    #[test]
    #[serial_test::serial(epics_env)]
    fn intf_addr_list_resolves_hostname_to_loopback() {
        unsafe {
            std::env::set_var("EPICS_PVA_INTF_ADDR_LIST", "localhost");
            std::env::set_var("EPICS_PVAS_INTF_ADDR_LIST", "localhost");
        }
        let client = list_intf_addresses();
        let server = server_intf_addr_list();
        unsafe {
            std::env::remove_var("EPICS_PVA_INTF_ADDR_LIST");
            std::env::remove_var("EPICS_PVAS_INTF_ADDR_LIST");
        }
        assert_eq!(
            client,
            vec![IpAddr::V4(Ipv4Addr::LOCALHOST)],
            "EPICS_PVA_INTF_ADDR_LIST=localhost must resolve to 127.0.0.1, not drop to []"
        );
        assert_eq!(
            server,
            vec![IpAddr::V4(Ipv4Addr::LOCALHOST)],
            "EPICS_PVAS_INTF_ADDR_LIST=localhost must resolve to 127.0.0.1, not drop to []"
        );
    }

    /// PVX-82: [`server_intf_addr_list_checked`] separates "unset / blank"
    /// (the operator wants the wildcard) from "named interface(s), none
    /// resolved" (a misconfiguration). The middle case is the
    /// security-relevant one — a typo'd bind restriction must surface as an
    /// error so the server refuses to start, NOT silently fall back to
    /// listening on every interface (`0.0.0.0`).
    #[test]
    #[serial_test::serial(epics_env)]
    fn server_intf_checked_errors_when_all_tokens_unresolvable() {
        // Unset → Ok(None): caller keeps its own interfaces.
        unsafe {
            std::env::remove_var("EPICS_PVA_INTF_ADDR_LIST");
            std::env::remove_var("EPICS_PVAS_INTF_ADDR_LIST");
        }
        assert_eq!(server_intf_addr_list_checked(), Ok(None));

        // Whitespace-only → Ok(None): no interface named ⟹ wildcard intent.
        unsafe {
            std::env::set_var("EPICS_PVAS_INTF_ADDR_LIST", "   ");
        }
        assert_eq!(server_intf_addr_list_checked(), Ok(None));

        // A resolvable interface → Ok(Some(addrs)).
        unsafe {
            std::env::set_var("EPICS_PVAS_INTF_ADDR_LIST", "127.0.0.1");
        }
        assert_eq!(
            server_intf_addr_list_checked(),
            Ok(Some(vec![IpAddr::V4(Ipv4Addr::LOCALHOST)]))
        );

        // Named interface(s) that all fail to resolve (RFC 6761 reserves
        // `.invalid` to always return NXDOMAIN) → Err: refuse the wildcard.
        unsafe {
            std::env::set_var("EPICS_PVAS_INTF_ADDR_LIST", "no-such-nic.invalid");
        }
        assert!(
            server_intf_addr_list_checked().is_err(),
            "an all-unresolvable INTF list must error, not drop to wildcard 0.0.0.0"
        );

        unsafe {
            std::env::remove_var("EPICS_PVAS_INTF_ADDR_LIST");
        }
    }

    /// PVX-82 (IGNORE sibling): the blocklist checked-resolver mirrors the
    /// INTF one — unset/blank ⟹ `Ok(None)`, ≥1 resolved ⟹ `Ok(Some)`, all
    /// non-blank tokens unresolvable ⟹ `Err` (refuse a silently-empty
    /// blocklist) rather than dropping every entry.
    #[test]
    #[serial_test::serial(epics_env)]
    fn server_ignore_checked_errors_when_all_tokens_unresolvable() {
        // Unset → Ok(None): caller keeps its own ignore_addrs.
        unsafe {
            std::env::remove_var("EPICS_PVAS_IGNORE_ADDR_LIST");
        }
        assert_eq!(server_ignore_addr_list_checked(), Ok(None));

        // Whitespace-only → Ok(None): no peer named ⟹ no blocklist.
        unsafe {
            std::env::set_var("EPICS_PVAS_IGNORE_ADDR_LIST", "   ");
        }
        assert_eq!(server_ignore_addr_list_checked(), Ok(None));

        // A resolvable peer (bare IP ⟹ wildcard port 0) → Ok(Some).
        unsafe {
            std::env::set_var("EPICS_PVAS_IGNORE_ADDR_LIST", "127.0.0.1");
        }
        assert_eq!(
            server_ignore_addr_list_checked(),
            Ok(Some(vec![(IpAddr::V4(Ipv4Addr::LOCALHOST), 0)]))
        );

        // Named peer(s) that all fail to resolve → Err: refuse the empty
        // blocklist instead of silently dropping the entries.
        unsafe {
            std::env::set_var("EPICS_PVAS_IGNORE_ADDR_LIST", "no-such-peer.invalid");
        }
        assert!(
            server_ignore_addr_list_checked().is_err(),
            "an all-unresolvable IGNORE list must error, not silently empty the blocklist"
        );

        unsafe {
            std::env::remove_var("EPICS_PVAS_IGNORE_ADDR_LIST");
        }
    }

    /// Interface lists deduplicate after resolution, preserving
    /// first-appearance order (pvxs normalizes then `removeDups`). A
    /// hostname that resolves to an IP already listed collapses into the
    /// existing entry.
    #[test]
    #[serial_test::serial(epics_env)]
    fn intf_addr_list_dedups_after_resolution() {
        unsafe {
            std::env::set_var("EPICS_PVA_INTF_ADDR_LIST", "127.0.0.1 localhost 127.0.0.1");
        }
        let client = list_intf_addresses();
        unsafe {
            std::env::remove_var("EPICS_PVA_INTF_ADDR_LIST");
        }
        assert_eq!(
            client,
            vec![IpAddr::V4(Ipv4Addr::LOCALHOST)],
            "literal 127.0.0.1 and resolved localhost must collapse to a single entry"
        );
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
    #[serial_test::serial(epics_env)]
    fn server_ignore_addr_list_resolves_hostname_tokens() {
        // SAFETY: std::env mutation is unsafe in edition 2024; the
        // `epics_env` serial guard makes it race-free under `cargo test`,
        // which runs all lib tests as threads in one process.
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

    /// `disable_plaintext` is parsed out of the server TLS options the
    /// way pvxs `parseTLSOptions` does (config.cpp:453-460): `true` /
    /// `false` set the flag, an unknown value is ignored, and an absent
    /// key yields `None` so `with_env` preserves the caller value.
    #[test]
    #[serial_test::serial(epics_env)]
    fn server_tls_disable_plaintext_parses_like_pvxs() {
        let prev_pvas = std::env::var("EPICS_PVAS_TLS_OPTIONS").ok();
        let prev_pva = std::env::var("EPICS_PVA_TLS_OPTIONS").ok();
        unsafe {
            std::env::remove_var("EPICS_PVAS_TLS_OPTIONS");
            std::env::remove_var("EPICS_PVA_TLS_OPTIONS");
        }
        // Absent key → None (preserve caller value).
        assert_eq!(server_tls_disable_plaintext_opt(), None);
        unsafe { std::env::set_var("EPICS_PVAS_TLS_OPTIONS", "client_cert=require") };
        assert_eq!(
            server_tls_disable_plaintext_opt(),
            None,
            "options present but no disable_plaintext key → None"
        );
        // Explicit true / false; coexists with other tokens.
        unsafe {
            std::env::set_var(
                "EPICS_PVAS_TLS_OPTIONS",
                "client_cert=require disable_plaintext=true",
            )
        };
        assert_eq!(server_tls_disable_plaintext_opt(), Some(true));
        unsafe { std::env::set_var("EPICS_PVAS_TLS_OPTIONS", "disable_plaintext=false") };
        assert_eq!(server_tls_disable_plaintext_opt(), Some(false));
        // Unknown value is ignored (pvxs logs + leaves the field unchanged).
        unsafe { std::env::set_var("EPICS_PVAS_TLS_OPTIONS", "disable_plaintext=maybe") };
        assert_eq!(server_tls_disable_plaintext_opt(), None);
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

    /// R16-32: every PVA timeout/period env double goes through
    /// `parse_timeout_env`, which reproduces pvxs `parse_timeout`
    /// (config.cpp:211-227) over `parseTo<double>` (util.cpp:769-783).
    #[test]
    fn parse_timeout_env_matches_pvxs_parse_timeout() {
        const N: &str = "EPICS_PVA_CONN_TMO";
        // Plain doubles.
        assert_eq!(parse_timeout_env(N, "30"), Some(30.0));
        assert_eq!(parse_timeout_env(N, "2.5"), Some(2.5));
        assert_eq!(parse_timeout_env(N, "1e3"), Some(1000.0));
        // stod skips leading whitespace; parseTo tolerates trailing.
        assert_eq!(parse_timeout_env(N, " 45"), Some(45.0));
        assert_eq!(parse_timeout_env(N, "45 "), Some(45.0));
        assert_eq!(parse_timeout_env(N, "\t45\n"), Some(45.0));
        // stod accepts C99 hex floats (binary exponent optional).
        assert_eq!(parse_timeout_env(N, "0x10"), Some(16.0));
        assert_eq!(parse_timeout_env(N, "0x1.8p1"), Some(3.0));
        assert_eq!(parse_timeout_env(N, "-0x1p2"), None, "negative → reject");
        // Out of range: > double(time_t::max) → pvxs throws out_of_range and
        // keeps the default. The port used to accept these and then PANIC in
        // Duration::from_secs_f64.
        assert_eq!(parse_timeout_env(N, "1e300"), None);
        assert_eq!(parse_timeout_env(N, "1e19"), None);
        assert_eq!(parse_timeout_env(N, "1e400"), None, "overflows to inf");
        // Non-finite / non-positive / garbage → keep default.
        assert_eq!(parse_timeout_env(N, "inf"), None);
        assert_eq!(parse_timeout_env(N, "nan"), None);
        assert_eq!(parse_timeout_env(N, "-1"), None);
        assert_eq!(parse_timeout_env(N, "0"), None);
        assert_eq!(parse_timeout_env(N, "45abc"), None, "extraneous chars");
        assert_eq!(parse_timeout_env(N, ""), None);
        // Anything accepted is convertible to a Duration, even after the
        // 4/3 tmoScale its owners apply — this is what closes the panic.
        let max = parse_timeout_env(N, &format!("{TIMEOUT_SECS_MAX}")).expect("time_t::max is ok");
        let _ = Duration::from_secs_f64(max * 4.0 / 3.0);
    }

    /// R16-32: a large-but-finite `EPICS_PVA_CONN_TMO` used to pass the
    /// `is_finite() && > 0.0` filter and panic every client/server at
    /// startup inside `Duration::from_secs_f64`. pvxs logs and keeps the
    /// default; so does the port now. Also covers the whitespace-tolerant
    /// values pvxs accepts but the port silently dropped.
    #[test]
    #[serial_test::serial(epics_env)]
    fn conn_tmo_out_of_range_keeps_default_instead_of_panicking() {
        let prev = std::env::var("EPICS_PVA_CONN_TMO").ok();
        unsafe {
            std::env::set_var("EPICS_PVA_CONN_TMO", "1e300");
        }
        assert_eq!(conn_timeout_secs_opt(), None, "out of range → keep default");
        assert_eq!(conn_timeout_secs(), 30.0);
        // The consumers' conversion is what panicked; exercise it.
        let _ = Duration::from_secs_f64((conn_timeout_secs() * 4.0 / 3.0).max(2.0));
        unsafe {
            std::env::set_var("EPICS_PVA_CONN_TMO", " 45 ");
        }
        assert_eq!(
            conn_timeout_secs_opt(),
            Some(45.0),
            "pvxs stod/parseTo tolerate surrounding whitespace"
        );
        unsafe {
            match prev {
                Some(v) => std::env::set_var("EPICS_PVA_CONN_TMO", v),
                None => std::env::remove_var("EPICS_PVA_CONN_TMO"),
            }
        }
    }

    /// R16-32 siblings: the port-only timeout/period doubles carry the same
    /// unguarded chain into `Duration::from_secs_f64`.
    #[test]
    #[serial_test::serial(epics_env)]
    fn port_only_timeout_vars_reject_out_of_range() {
        let names = [
            "EPICS_PVAS_BEACON_PERIOD",
            "EPICS_PVAS_BEACON_PERIOD_LONG",
            "EPICS_PVAS_SEND_TMO",
            "EPICS_PVAS_TLS_HANDSHAKE_TMO",
        ];
        let prev: Vec<_> = names.iter().map(|n| std::env::var(n).ok()).collect();
        unsafe {
            for n in names {
                std::env::set_var(n, "1e300");
            }
        }
        assert_eq!(beacon_period_opt(), None);
        assert_eq!(beacon_period(), Duration::from_secs(15));
        assert_eq!(beacon_period_long(), None);
        assert_eq!(send_timeout_secs_opt(), None);
        assert_eq!(send_timeout_secs(), 5.0);
        assert_eq!(tls_handshake_timeout_secs_opt(), None);
        assert_eq!(tls_handshake_timeout_secs(), 10.0);
        unsafe {
            for n in names {
                std::env::set_var(n, " 3 ");
            }
        }
        assert_eq!(beacon_period_opt(), Some(Duration::from_secs(3)));
        assert_eq!(beacon_period_long(), Some(Duration::from_secs(3)));
        assert_eq!(send_timeout_secs_opt(), Some(3.0));
        assert_eq!(tls_handshake_timeout_secs_opt(), Some(3.0));
        unsafe {
            for (n, p) in names.iter().zip(prev) {
                match p {
                    Some(v) => std::env::set_var(n, v),
                    None => std::env::remove_var(n),
                }
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

    /// An explicit
    /// `host:0` in `EPICS_PVA_NAME_SERVERS` must normalize to the effective
    /// client TCP port (5075), not survive as a literal port-0 TCP
    /// destination — pvxs `split_addr_into` (`config.cpp:167-168`) sets the
    /// list default on any `port==0` token. This holds both with default env
    /// and under `EPICS_PVA_SERVER_PORT=0` (which expands to 5075). A
    /// non-zero explicit port is still preserved verbatim.
    #[test]
    #[serial_test::serial(epics_env)]
    fn name_servers_explicit_zero_port_normalizes_to_effective_tcp_port() {
        let prev_pva = std::env::var("EPICS_PVA_SERVER_PORT").ok();
        let prev_pvas = std::env::var("EPICS_PVAS_SERVER_PORT").ok();
        let prev_ns = std::env::var("EPICS_PVA_NAME_SERVERS").ok();

        // Default env (no server-port override): `:0` → 5075.
        unsafe {
            std::env::remove_var("EPICS_PVA_SERVER_PORT");
            std::env::remove_var("EPICS_PVAS_SERVER_PORT");
            std::env::set_var("EPICS_PVA_NAME_SERVERS", "127.0.0.1:0");
        }
        assert_eq!(
            name_servers()
                .iter()
                .map(SocketAddr::port)
                .collect::<Vec<_>>(),
            vec![5075],
            "explicit `:0` must normalize to the effective TCP port, not 0"
        );

        // EPICS_PVA_SERVER_PORT=0 expands to 5075 (server_port zero rule),
        // and `:0` resolves through it to 5075.
        unsafe {
            std::env::set_var("EPICS_PVA_SERVER_PORT", "0");
        }
        assert_eq!(
            name_servers()
                .iter()
                .map(SocketAddr::port)
                .collect::<Vec<_>>(),
            vec![5075],
            "explicit `:0` under EPICS_PVA_SERVER_PORT=0 must still resolve to 5075"
        );

        // A non-zero explicit port survives unchanged.
        unsafe {
            std::env::set_var("EPICS_PVA_NAME_SERVERS", "127.0.0.1:9876");
        }
        assert_eq!(
            name_servers()
                .iter()
                .map(SocketAddr::port)
                .collect::<Vec<_>>(),
            vec![9876],
            "a non-zero explicit name-server port is preserved verbatim"
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

    // ---- Config::expand() ----

    /// Build a bare client config (no env reads) for `expand()` unit tests.
    fn bare_config() -> Config {
        Config {
            udp_port: 5076,
            tcp_port: 5075,
            auto_addr_list: false,
            auto_beacon: false,
            interfaces: Vec::new(),
            address_list: Vec::new(),
            name_servers: Vec::new(),
            beacon_destinations: Vec::new(),
            ignore_addrs: Vec::new(),
            tcp_timeout: 40.0,
        }
    }

    /// `expand()` promotes a zero UDP/TCP port to the protocol defaults —
    /// pvxs `config.cpp:563-566, 575-578, 628-632` (a zero effective port is
    /// never a usable destination).
    #[test]
    fn config_expand_promotes_zero_ports() {
        let mut c = bare_config();
        c.udp_port = 0;
        c.tcp_port = 0;
        c.expand();
        assert_eq!(c.udp_port, 5076);
        assert_eq!(c.tcp_port, 5075);
    }

    /// An empty interface list expands to the wildcard `0.0.0.0` — pvxs
    /// "no addresses isn't interesting" (`config.cpp:492-494, 637-638`).
    #[test]
    fn config_expand_fills_wildcard_interface() {
        let mut c = bare_config();
        c.expand();
        assert!(
            c.interfaces
                .iter()
                .any(|e| e.addr.ip() == IpAddr::V4(Ipv4Addr::UNSPECIFIED)),
            "empty interface list must expand to the wildcard: {:?}",
            c.interfaces
        );
    }

    /// `auto_addr_list` appends the limited broadcast to the SEARCH targets
    /// and then clears the flag — pvxs `autoAddrList` consumption
    /// (`config.cpp:640-643`). The flag is idempotent: a second `expand()`
    /// adds nothing.
    #[test]
    fn config_expand_consumes_auto_addr_list() {
        let mut c = bare_config();
        c.auto_addr_list = true;
        c.expand();
        assert!(
            !c.auto_addr_list,
            "auto_addr_list must be cleared after expand"
        );
        let bcast = SocketAddr::new(IpAddr::V4(Ipv4Addr::BROADCAST), 5076);
        assert!(
            c.address_list.iter().any(|e| e.addr == bcast),
            "auto expansion must add the limited broadcast: {:?}",
            c.address_list
        );
        let n = c.address_list.len();
        c.expand();
        assert_eq!(
            n,
            c.address_list.len(),
            "re-expand must not duplicate targets"
        );
    }

    /// Duplicate destinations collapse to one, keeping the longest TTL and
    /// first-seen order — pvxs `removeDups` (`config.cpp:349-371, 647`).
    #[test]
    fn config_expand_dedups_endpoints_longest_ttl() {
        let mut c = bare_config();
        let mcast = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(224, 0, 2, 3)), 5076);
        c.address_list = vec![
            Endpoint {
                addr: mcast,
                ttl: Some(1),
                iface: None,
            },
            Endpoint {
                addr: mcast,
                ttl: Some(8),
                iface: None,
            },
        ];
        c.expand();
        assert_eq!(
            c.address_list.len(),
            1,
            "duplicate (addr,iface) must collapse"
        );
        assert_eq!(c.address_list[0].ttl, Some(8), "longest TTL must win");
    }

    /// A port-less destination takes the effective UDP port before dedup —
    /// pvxs `split_addr_into` (`config.cpp:167-168`).
    #[test]
    fn config_expand_sets_default_port_on_targets() {
        let mut c = bare_config();
        c.udp_port = 5099;
        c.address_list = vec![Endpoint::from(SocketAddr::new(
            IpAddr::V4(Ipv4Addr::new(10, 0, 0, 5)),
            0,
        ))];
        c.expand();
        assert_eq!(c.address_list[0].addr.port(), 5099);
    }

    /// The idle timeout is clamped to the pvxs `enforceTimeout` bounds
    /// (`config.cpp:373-391`): non-positive → 40 s default, below the 2 s
    /// floor → 2 s, a sane value is preserved.
    #[test]
    fn config_expand_enforces_timeout_bounds() {
        let mut zero = bare_config();
        zero.tcp_timeout = 0.0;
        zero.expand();
        assert_eq!(zero.tcp_timeout, 40.0);

        let mut tiny = bare_config();
        tiny.tcp_timeout = 1.0;
        tiny.expand();
        assert_eq!(tiny.tcp_timeout, 2.0);

        let mut sane = bare_config();
        sane.tcp_timeout = 40.0;
        sane.expand();
        assert_eq!(sane.tcp_timeout, 40.0);

        let mut nan = bare_config();
        nan.tcp_timeout = f64::NAN;
        nan.expand();
        assert_eq!(nan.tcp_timeout, 40.0);
    }

    /// `echo_period_secs` is pvxs's `max(1, min(15, tcpTimeout*3/8))`
    /// (clientconn.cpp:163). Boundaries: the documented 40 s → 15 s pair,
    /// the interior, and both bounds. The 15 s CAP is the half the port had
    /// dropped (R17-36).
    #[test]
    fn echo_period_is_bounded_to_one_through_fifteen_seconds() {
        // pvxs's own worked example: "tcpTimeout(40) -> 15 second echo".
        assert_eq!(echo_period_secs(40.0), 15.0);
        // Interior: the plain 3/8.
        assert_eq!(echo_period_secs(8.0), 3.0);
        // Upper bound: anything above 40 s stays at the 15 s cap.
        assert_eq!(echo_period_secs(133.0), 15.0);
        assert_eq!(echo_period_secs(1e18), 15.0);
        // Lower bound: below 8/3 s the period floors at 1 s.
        assert_eq!(echo_period_secs(2.0), 1.0);
        assert_eq!(echo_period_secs(0.0), 1.0);
    }

    /// `effective_tcp_timeout_secs` is the single owner of "configured
    /// CONN_TMO → effective idle window": `tmoScale` (4/3) then pvxs
    /// `enforceTimeout` (config.cpp:373-391), which has BOTH an upper
    /// reset and a lower floor. Boundaries, one per case.
    #[test]
    fn effective_tcp_timeout_applies_both_enforce_timeout_bounds() {
        // Default: 30 × 4/3 = 40 s, pvxs's documented effective timeout.
        assert_eq!(effective_tcp_timeout_secs(30.0), 40.0);

        // Lower floor: 1.0 × 4/3 = 1.333 < 2 → 2 s.
        assert_eq!(effective_tcp_timeout_secs(1.0), 2.0);
        // Exactly at the floor after scaling.
        assert_eq!(effective_tcp_timeout_secs(1.5), 2.0);
        // Above the floor: the scaled double survives verbatim.
        assert_eq!(effective_tcp_timeout_secs(2.5), 2.5 * TMO_SCALE);

        // Upper reset. `parse_timeout` ACCEPTS a configured value up to
        // `time_t::max` (~9.22e18), and `enforceTimeout` then runs on the
        // SCALED value — so any configured value above 3/4 of that (here
        // 7e18 × 4/3 ≈ 9.33e18 ≥ time_t::max) resets to 40 s. Pre-fix the
        // scale-then-floor sites kept the ~9.33e18 s window.
        let accepted = 7e18_f64.min(TIMEOUT_SECS_MAX);
        assert_eq!(accepted, 7e18, "7e18 must be an ACCEPTED CONN_TMO");
        assert_eq!(effective_tcp_timeout_secs(accepted), 40.0);
        assert_eq!(effective_tcp_timeout_secs(TIMEOUT_SECS_MAX), 40.0);

        // Non-positive / non-finite also reset to 40 s.
        assert_eq!(effective_tcp_timeout_secs(0.0), 40.0);
        assert_eq!(effective_tcp_timeout_secs(-1.0), 40.0);
        assert_eq!(effective_tcp_timeout_secs(f64::INFINITY), 40.0);
        assert_eq!(effective_tcp_timeout_secs(f64::NAN), 40.0);
    }

    /// `from_client_env` reads `EPICS_PVA_*`, scaling `CONN_TMO` by 4/3 and
    /// parsing the address list against the UDP port (pvxs
    /// `client::Config::fromEnv`, `config.cpp:552-599`).
    #[test]
    #[serial_test::serial(epics_env)]
    fn config_from_client_env_reads_pva_vars() {
        let keys = [
            "EPICS_PVA_BROADCAST_PORT",
            "EPICS_PVA_SERVER_PORT",
            "EPICS_PVA_ADDR_LIST",
            "EPICS_PVA_AUTO_ADDR_LIST",
            "EPICS_PVA_CONN_TMO",
            "EPICS_PVA_INTF_ADDR_LIST",
            "EPICS_PVA_NAME_SERVERS",
        ];
        let saved: Vec<_> = keys.iter().map(|k| std::env::var(k).ok()).collect();
        // SAFETY: std::env mutation is unsafe in edition 2024; the
        // `epics_env` serial guard makes the whole block race-free.
        unsafe {
            std::env::set_var("EPICS_PVA_BROADCAST_PORT", "5099");
            std::env::set_var("EPICS_PVA_SERVER_PORT", "5098");
            std::env::set_var("EPICS_PVA_ADDR_LIST", "10.0.0.5");
            std::env::set_var("EPICS_PVA_AUTO_ADDR_LIST", "NO");
            std::env::set_var("EPICS_PVA_CONN_TMO", "30");
            std::env::remove_var("EPICS_PVA_INTF_ADDR_LIST");
            std::env::remove_var("EPICS_PVA_NAME_SERVERS");
        }
        let c = Config::from_client_env();
        assert_eq!(c.udp_port, 5099);
        assert_eq!(c.tcp_port, 5098);
        assert!(!c.auto_addr_list);
        // CONN_TMO 30 s scaled by 4/3 → 40 s effective.
        assert!((c.tcp_timeout - 40.0).abs() < 1e-9);
        assert!(
            c.address_list
                .iter()
                .any(|e| e.addr == SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 5)), 5099)),
            "addr list must parse against the UDP port: {:?}",
            c.address_list
        );
        // SAFETY: restore, same serial guard.
        unsafe {
            for (k, v) in keys.iter().zip(saved) {
                match v {
                    Some(val) => std::env::set_var(k, val),
                    None => std::env::remove_var(k),
                }
            }
        }
    }
}
