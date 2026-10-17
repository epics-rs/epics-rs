//! Environment-variable parsers for `EPICS_PVA_*` / `EPICS_PVAS_*`.
//!
//! Pure functions — they read the environment directly so the
//! caller doesn't need to thread a Config struct. Where pvxs has
//! Config::fromEnv() that builds an internal config object, we expose
//! one helper per variable. Server-side helpers fall back to the
//! corresponding client-side variable when the `EPICS_PVAS_*` form is
//! not set, matching pvxs's `Config::server()` behavior.

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::net::Ipv4Addr;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::time::Duration;

use epics_base_rs::runtime::env;
use epics_base_rs::runtime::env::get_lossy as getenv;

/// Expand `$(VAR)` and `${VAR}` references in `input` against the
/// process environment. Mirrors pvxs `Config::expand()` (server.h:219).
/// Unrecognised refs (no env entry) substitute to an empty string —
/// matching the C IOC `dbLoadRecords` macro-expansion behaviour. A
/// literal `$` followed by non-`(`/`{` is preserved verbatim.
pub fn expand_dollar_vars(input: impl AsRef<OsStr>) -> OsString {
    // Bytes, as pvxs's `std::string`: a value is `getenv`'s, spliced in
    // unchecked, and every cut below is at an ASCII byte.
    let bytes = input.as_ref().as_encoded_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let close = match bytes.get(i + 1) {
            Some(b'(') if bytes[i] == b'$' => Some(b')'),
            Some(b'{') if bytes[i] == b'$' => Some(b'}'),
            _ => None,
        };
        let Some(end) = close else {
            out.push(bytes[i]);
            i += 1;
            continue;
        };
        let name = &bytes[i + 2..];
        match name.iter().position(|&b| b == end) {
            Some(j) => {
                // SAFETY: cut from `input`'s encoded bytes at ASCII bytes.
                let name = unsafe { OsStr::from_encoded_bytes_unchecked(&name[..j]) };
                // Unset → empty (pvxs Config::expand parity).
                if let Some(val) = epics_base_rs::runtime::env::get(name) {
                    out.extend_from_slice(val.as_encoded_bytes());
                }
                i += 3 + j;
            }
            None => {
                // Unterminated $( … — emit as-is including the open
                // bracket so the caller's parser can fail loudly
                // rather than silently consuming text.
                out.extend_from_slice(&bytes[i..]);
                i = bytes.len();
            }
        }
    }
    // SAFETY: `out` is `input`'s bytes and whole env values, joined at ASCII
    // bytes.
    unsafe { OsString::from_encoded_bytes_unchecked(out) }
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
        let exists = std::env::var_os(name).is_some();
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
/// implemented in `src/config.cpp:468-472` / `:607-611`): it captures the
/// supplied definitions **into this object** and leaves the process
/// environment unchanged. Two `PvaConfigDefs` built from two different
/// maps are fully independent — building one does not affect the other or
/// any [`seed_env_overrides`]-seeded global, so a single process can hold
/// several named client/server configs (pva2pva-style) without
/// cross-contamination.
///
/// Scoped definitions and the ambient environment are two SEPARATE
/// sources, mirroring pvxs's `PickOne{useenv}` (config.cpp:229-255):
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
    /// `getenv()` (config.cpp:229-255, :468-472, :607-611); an absent key
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

/// Parse a `EPICS_PVA_ADDR_LIST`-style string into a list of
/// `SocketAddr`, discarding any per-entry multicast modifiers. Entries are
/// **whitespace-separated** (pvxs `split_addr_into`, `config.cpp:151-183`);
/// each is an `<addr>[,ttl][@iface]` [`Endpoint`] whose address is kept.
/// Accepts plain IPs (gets `default_port` appended), `ip:port`, DNS
/// hostnames, and `hostname:port`; an invalid entry is dropped with
/// [`parse_endpoints_with_port`]'s warning. The environment's lists are read
/// by `split_addr_list` instead.
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
/// (`evhelper.cpp:556-592`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    /// Resolved destination (IP + port).
    pub addr: SocketAddr,
    /// Multicast TTL (`,<n>` modifier), pvxs's `int ttl=-1`
    /// (`osiSockExt.h:142`): `parseTo<int64_t>` narrowed to `int`, -1 when the
    /// token names none. Applied only when `addr` is a multicast group.
    pub ttl: i32,
    /// Outgoing interface (`@name-or-ip` modifier), verbatim from the
    /// token. `None` = OS default route. Applied only for multicast.
    pub iface: Option<String>,
}

impl Endpoint {
    /// Parse one address-list entry, pvxs `SockEndpoint::SockEndpoint`
    /// (`config.cpp:32-61`) with `split_addr_into`'s port-0 fill
    /// (`config.cpp:167-168`), failing with the `what()` pvxs's lists print.
    pub fn parse(token: &str, default_port: u16) -> Result<Endpoint, String> {
        let comma = token.find(',');
        let at = token.find('@');
        if let (Some(c), Some(a)) = (comma, at) {
            if c > a {
                return Err(format!(
                    "\"{}\" comma expected before @",
                    crate::pvdata::fmt::escape(token)
                ));
            }
        }
        let addr_str = &token[..comma.or(at).unwrap_or(token.len())];
        let mut addr = super::sockaddr::set_address(addr_str, default_port)?;
        if addr.port() == 0 {
            addr.set_port(default_port);
        }
        let ttl_str = match (comma, at) {
            (Some(c), Some(a)) => Some(&token[c + 1..a]),
            (Some(c), None) => Some(&token[c + 1..]),
            _ => None,
        };
        let ttl = match ttl_str {
            Some(s) => crate::pvdata::convert::parse_to_i64(s).map_err(|e| e.to_string())? as i32,
            None => -1,
        };
        let iface = at
            .map(|a| token[a + 1..].to_string())
            .filter(|i| !i.is_empty());
        let iface = iface.and_then(|i| super::IfaceMap::current().normalize(addr, Some(i)));
        Ok(Endpoint { addr, ttl, iface })
    }

    /// The TTL / hop limit pvxs `mcast_prep_sendto` sets for this endpoint
    /// (`evhelper.cpp:588`, `:601`): its `ttl` default `-1` is the kernel's 1.
    /// Any other value goes as the same `int` bits.
    pub fn mcast_ttl(&self) -> u32 {
        match self.ttl {
            -1 => 1,
            ttl => ttl as u32,
        }
    }

    /// The v4 interface pvxs joins and sends this endpoint on
    /// (`config.cpp:101-106`, `evhelper.cpp:584-592`), `0.0.0.0` when it
    /// names none.
    pub fn mcast_iface_v4(&self, ifmap: &super::IfaceMap) -> Ipv4Addr {
        self.iface
            .as_deref()
            .map_or(Ipv4Addr::UNSPECIFIED, |n| ifmap.address_of_v4(n))
    }

    /// The v6 interface index the same way (`config.cpp:112-117`,
    /// `evhelper.cpp:597-606`), 0 when it names none.
    pub fn mcast_iface_v6(&self, ifmap: &super::IfaceMap) -> u32 {
        self.iface.as_deref().map_or(0, |n| ifmap.index_of(n))
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
            ttl: -1,
            iface: None,
        }
    }
}

/// Endpoint-preserving variant of [`parse_addr_list_with_port`]: splits on
/// WHITESPACE only (pvxs `split_addr_into`, `config.cpp:151-183` — the
/// comma is multicast-TTL syntax, never a list separator) and keeps each
/// entry's multicast TTL / interface modifiers for the UDP send path.
///
/// This is a caller's list, not the environment's, so an invalid entry is
/// what pvxs `parseAddresses` warns about and skips when `expand()` meets it
/// (`config.cpp:257-269`).
pub fn parse_endpoints_with_port(env: &str, default_port: u16) -> Vec<Endpoint> {
    split_tokens(env, default_port)
        .filter_map(|(token, ep)| match ep {
            Ok(ep) => Some(ep),
            Err(what) => {
                crate::pvxs_log::log(
                    crate::pvxs_log::CONFIG,
                    crate::pvxs_log::Level::Warn,
                    format_args!("Ignoring {token} : {what}"),
                );
                None
            }
        })
        .collect()
}

/// The entries of an address list, `$(VAR)` expanded (PVA-466), each parsed
/// as pvxs `split_addr_into` parses it (`config.cpp:159-170`), lazily so a
/// required list stops at its first invalid entry as pvxs's throw does.
fn split_tokens(
    raw: &str,
    default_port: u16,
) -> impl Iterator<Item = (String, Result<Endpoint, String>)> {
    let raw = expand_dollar_vars(raw).to_string_lossy().into_owned();
    let tokens: Vec<String> = raw.split_whitespace().map(str::to_owned).collect();
    tokens.into_iter().map(move |token| {
        let ep = Endpoint::parse(&token, default_port);
        (token, ep)
    })
}

/// pvxs `split_addr_into` for a list it reads with `required=false`
/// (`config.cpp:175`): an invalid entry is logged on `pvxs.config` and
/// skipped, the rest come back in [`split_order`].
pub(crate) fn split_addr_list(name: &str, raw: &str, default_port: u16) -> Vec<Endpoint> {
    let eps = split_tokens(raw, default_port)
        .filter_map(|(token, ep)| match ep {
            Ok(ep) => Some(ep),
            Err(what) => {
                crate::pvxs_log::log(
                    crate::pvxs_log::CONFIG,
                    crate::pvxs_log::Level::Err,
                    format_args!("{name} ignoring invalid '{token}' : {what}"),
                );
                None
            }
        })
        .collect();
    split_order(eps, default_port)
}

/// pvxs `split_addr_into` for a list it reads with `required=true`, the
/// server's interface and ignore lists (`config.cpp:418-424`): the first
/// invalid entry fails the whole configuration (`config.cpp:172-173`).
/// `default_port` goes into the [`split_order`] key only: an entry naming no
/// port keeps 0, so the server's resolved port still reaches the bind.
pub(crate) fn split_addr_list_required(
    raw: &str,
    default_port: u16,
) -> Result<Vec<Endpoint>, String> {
    let eps = split_tokens(raw, 0)
        .map(|(token, ep)| ep.map_err(|what| format!("invalid endpoint \"{token}\" {what}")))
        .collect::<Result<_, _>>()?;
    Ok(split_order(eps, default_port))
}

/// The tail of pvxs `split_addr_into` (`config.cpp:166-182`): each endpoint
/// printed with `default_port` filled in, the list sorted and deduped by
/// those strings. Order is load-bearing on the server, whose first interface
/// is the one that may fall back (`server.cpp:476-486`), and a duplicate
/// entry is a duplicate search, beacon or name-server connection.
fn split_order(eps: Vec<Endpoint>, default_port: u16) -> Vec<Endpoint> {
    let mut printed: Vec<(String, Endpoint)> = eps
        .into_iter()
        .map(|ep| {
            let mut key = ep.clone();
            if key.addr.port() == 0 {
                key.addr.set_port(default_port);
            }
            (sock_endpoint_text(&key), ep)
        })
        .collect();
    printed.sort_by(|a, b| a.0.cmp(&b.0));
    printed.dedup_by(|a, b| a.0 == b.0);
    printed.into_iter().map(|(_, ep)| ep).collect()
}

/// `split_addr_into` onto a list that already holds entries, as
/// `Config::applyEnv` does with a caller's (`config.cpp:151-183`): `add` is
/// appended and the whole list put in [`split_order`].
pub(crate) fn split_addr_into(out: &mut Vec<Endpoint>, add: Vec<Endpoint>, default_port: u16) {
    out.extend(add);
    *out = split_order(std::mem::take(out), default_port);
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
/// TTL 8, and `@iface` duplicates double-beaconed a NIC.
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
        crate::pvxs_log::log(
            crate::pvxs_log::CONFIG,
            crate::pvxs_log::Level::Err,
            format_args!("{name} invalid bool value (YES/NO) : '{raw}'"),
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
        // pvxs takes zero without a word.
        Some(0.0) => None,
        _ => {
            crate::pvxs_log::log(
                crate::pvxs_log::SERVER_SETUP,
                crate::pvxs_log::Level::Err,
                format_args!("{name} invalid double value : '{raw}'"),
            );
            None
        }
    }
}

/// A port value as pvxs `_fromDefs` parses it: `parseTo<uint64_t>`
/// assigned into the `unsigned short` port field (`src/pvxs/server.h:168/170`,
/// `src/pvxs/client.h:1030/1033`), so `70000` is `4464`. The error is
/// `parseTo`'s `what()`.
pub fn parse_port_env(raw: &str) -> Result<u16, String> {
    crate::pvdata::convert::parse_to_u64(raw)
        .map(|v| v as u16)
        .map_err(|e| e.to_string())
}

/// pvxs `PickOne` (`config.cpp:229-255`) over port variables: the first of
/// `names` that is set, parsed by [`parse_port_env`]. The error is the
/// `"<name> invalid integer : <what>"` line the `fromEnv` owners log.
pub(crate) fn pick_port(names: &[&'static str]) -> Option<Result<u16, String>> {
    let (name, raw) = names.iter().find_map(|n| getenv(n).map(|v| (*n, v)))?;
    Some(parse_port_env(&raw).map_err(|what| format!("{name} invalid integer : {what}")))
}

/// pvxs client `_fromDefs` ports (`config.cpp:556-578`) and the zero
/// `tcp_port` `Config::expand` restores (`config.cpp:631-632`), as
/// `(udp_port, tcp_port)`. `warn` gets each line pvxs logs at WARN on
/// `pvxs.client.setup`, which only the `fromEnv` owners log
/// ([`client_ports_logged`]).
pub(crate) fn client_ports(mut warn: impl FnMut(&str)) -> (u16, u16) {
    let udp = match pick_port(&["EPICS_PVA_BROADCAST_PORT"]) {
        Some(Ok(0)) => {
            warn("ignoring EPICS_PVA_BROADCAST_PORT=0");
            5076
        }
        Some(Ok(p)) => p,
        Some(Err(e)) => {
            warn(&e);
            5076
        }
        None => 5076,
    };
    // pvxs's `tcp_port==0 && !nameServers.empty()` warning cannot fire
    // here: `fromEnv` reads the name servers after the port.
    let tcp = match pick_port(&["EPICS_PVA_SERVER_PORT", "EPICS_PVAS_SERVER_PORT"]) {
        Some(Ok(p)) if p != 0 => p,
        Some(Err(e)) => {
            warn(&e);
            5075
        }
        _ => 5075,
    };
    (udp, tcp)
}

/// [`client_ports`] as a client `fromEnv` reads them, logging.
pub(crate) fn client_ports_logged() -> (u16, u16) {
    client_ports(|line| {
        crate::pvxs_log::log(
            crate::pvxs_log::CLIENT_SETUP,
            crate::pvxs_log::Level::Warn,
            format_args!("{line}"),
        )
    })
}

/// One server port as server `_fromDefs` reads it (`config.cpp:402-416`):
/// set from the first of `names` set, or logged at ERR and left as it was.
pub(crate) fn server_port_logged(names: &[&'static str], port: &mut u16) {
    match pick_port(names) {
        Some(Ok(v)) => *port = v,
        Some(Err(line)) => crate::pvxs_log::log(
            crate::pvxs_log::SERVER_SETUP,
            crate::pvxs_log::Level::Err,
            format_args!("{line}"),
        ),
        None => {}
    }
}

/// The client UDP search/beacon port, `client_ports`'s `udp_port`:
/// `EPICS_PVA_BROADCAST_PORT`, 5076 when unset, invalid or zero.
pub fn broadcast_port() -> u16 {
    client_ports(|_| ()).0
}

/// `EPICS_PVAS_BROADCAST_PORT` falling back to `EPICS_PVA_BROADCAST_PORT`,
/// `None` when neither is set or the picked one is invalid, so
/// [`crate::server_native::PvaServerConfig::with_env`] keeps a
/// caller-supplied port (pvxs `PickOne`, `config.cpp:410-416`).
pub fn server_broadcast_port_opt() -> Option<u16> {
    pick_port(SERVER_BROADCAST_PORT_VARS)?.ok()
}

/// `EPICS_PVAS_BROADCAST_PORT` falling back to `EPICS_PVA_BROADCAST_PORT`.
pub fn server_broadcast_port() -> u16 {
    server_broadcast_port_opt().unwrap_or(5076)
}

/// The server's `PickOne` order for its TCP port (`config.cpp:402`).
pub(crate) const SERVER_PORT_VARS: &[&str] = &["EPICS_PVAS_SERVER_PORT", "EPICS_PVA_SERVER_PORT"];
/// The server's `PickOne` order for its UDP port (`config.cpp:410`).
pub(crate) const SERVER_BROADCAST_PORT_VARS: &[&str] =
    &["EPICS_PVAS_BROADCAST_PORT", "EPICS_PVA_BROADCAST_PORT"];

/// The client default TCP destination port, `client_ports`'s `tcp_port`:
/// the port a port-less `EPICS_PVA_NAME_SERVERS` entry takes. Zero is a
/// server's ephemeral bind but never a client destination, so it is 5075.
pub fn server_port() -> u16 {
    client_ports(|_| ()).1
}

/// The server TCP port: `EPICS_PVAS_SERVER_PORT`, then
/// `EPICS_PVA_SERVER_PORT`, then 5075 (pvxs `config.cpp:402-408`).
pub fn pvas_server_port() -> u16 {
    pvas_server_port_opt().unwrap_or(5075)
}

/// Presence-aware [`pvas_server_port`] — `None` when neither variable is
/// set or the picked one is invalid, so `with_env` keeps a caller-supplied
/// `tcp_port`.
pub fn pvas_server_port_opt() -> Option<u16> {
    pick_port(SERVER_PORT_VARS)?.ok()
}

/// Presence-aware server **TLS** listen port — `EPICS_PVAS_TLS_PORT`
/// first, then the shared `EPICS_PVA_TLS_PORT`, returning `None` when
/// neither is set so [`crate::server_native::PvaServerConfig::with_env`]
/// preserves a caller-supplied `tls_port`.
///
/// Mirrors pvxs `server::Config::_fromDefs` `PickOne` precedence
/// (`config.cpp:495-501`, `tls` `b3a10bf0`): the server reads the
/// server-specific `EPICS_PVAS_TLS_PORT` first and falls back to the shared
/// `EPICS_PVA_TLS_PORT`. The compiled default (pvxs `netcommon.h:133`,
/// `tls_port = 5076`) lives on
/// [`crate::server_native::PvaServerConfig::default`], not here, so an absent
/// variable never overwrites the caller value.
pub fn pvas_tls_port_opt() -> Option<u16> {
    pick_port(&["EPICS_PVAS_TLS_PORT", "EPICS_PVA_TLS_PORT"])?.ok()
}

/// `EPICS_PVA_AUTO_ADDR_LIST` — default YES. When truthy, the search
/// engine adds per-NIC broadcast addresses to the SEARCH targets list.
pub fn auto_addr_list_enabled() -> bool {
    match getenv("EPICS_PVA_AUTO_ADDR_LIST") {
        // Present-but-invalid preserves the default (`true`), matching
        // pvxs which leaves `autoAddrList` untouched on a bad value.
        Some(v) => parse_bool("EPICS_PVA_AUTO_ADDR_LIST", &v).unwrap_or(true),
        None => true,
    }
}

/// `EPICS_PVAS_AUTO_BEACON_ADDR_LIST` — default YES. When truthy,
/// beacons fan out to each interface's limited broadcast (255.255.255.255
/// scoped to the NIC).
///
/// pvxs `config.cpp:426-432` falls back to
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
    let (name, raw) = getenv("EPICS_PVAS_AUTO_BEACON_ADDR_LIST")
        .map(|v| ("EPICS_PVAS_AUTO_BEACON_ADDR_LIST", v))
        .or_else(|| getenv("EPICS_PVA_AUTO_ADDR_LIST").map(|v| ("EPICS_PVA_AUTO_ADDR_LIST", v)))?;
    parse_bool(name, &raw)
}

/// `EPICS_PVAS_BEACON_PERIOD` — default 15s. Controls the *short*
/// burst-interval; see [`crate::server_native::PvaServerConfig`]
/// for the burst-then-slowdown semantics. Rust extension — pvxs has no
/// configurable beacon interval (fixed 15s/180s, `src/server.cpp:45-46`).
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
    getenv("EPICS_PVAS_BEACON_PERIOD")
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
    getenv("EPICS_PVAS_BEACON_PERIOD_LONG")
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
    getenv("EPICS_PVAS_MAX_CONNECTIONS")
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
    getenv("EPICS_PVA_CONN_TMO").and_then(|s| parse_timeout_env("EPICS_PVA_CONN_TMO", &s))
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
    getenv("EPICS_PVAS_SEND_TMO")
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
    getenv("EPICS_PVAS_TLS_HANDSHAKE_TMO")
        .and_then(|s| parse_timeout_env("EPICS_PVAS_TLS_HANDSHAKE_TMO", &s))
        .map(|v| v.max(1.0))
}

/// Non-pvxs fallback password for a server-side TLS keychain.
///
/// pvxs has no `*_TLS_KEYCHAIN_PASSWORD` variable: it sources the
/// PKCS#12 password solely from the `;password` suffix of the keychain
/// spec (`ossl.cpp:223-229` on pvxs `tls` `b3a10bf0`, see
/// [`split_keychain_spec`]). This helper is a Rust-only convenience
/// consulted ONLY when the keychain spec
/// carries no inline `;` suffix. It reads
/// `EPICS_PVAS_TLS_KEYCHAIN_PASSWORD`, falling back to
/// `EPICS_PVA_TLS_KEYCHAIN_PASSWORD`. `$(VAR)` / `${VAR}` refs are
/// expanded (PVA-466) so operators can template
/// `EPICS_PVAS_TLS_KEYCHAIN_PASSWORD="$(SECRET_DIR)/..."` style
/// indirections. Returns `None` when neither variable is set; an empty
/// string is preserved as `Some("")` (a deliberately password-less
/// PKCS#12 is distinct from "unset").
pub fn server_tls_keychain_password() -> Option<OsString> {
    env::get("EPICS_PVAS_TLS_KEYCHAIN_PASSWORD")
        .or_else(|| env::get("EPICS_PVA_TLS_KEYCHAIN_PASSWORD"))
        .map(expand_dollar_vars)
}

/// Keychain path for a server-side TLS endpoint.
///
/// Reads `EPICS_PVAS_TLS_KEYCHAIN`, falling back to `EPICS_PVA_TLS_KEYCHAIN`
/// when the server-specific form is unset — matching pvxs `Config::server()`
/// `pickone({"EPICS_PVAS_TLS_KEYCHAIN", "EPICS_PVA_TLS_KEYCHAIN"})`
/// (`config.cpp:479`, `tls` `b3a10bf0`), which takes the first variable
/// present. A server configured with only the shared `EPICS_PVA_TLS_KEYCHAIN`
/// must still enable TLS (pvxs does); reading the server-specific form alone
/// left such a server with TLS silently disabled. The client path stays
/// `EPICS_PVA_TLS_KEYCHAIN`-only (pvxs `config.cpp:654`, `tls` `b3a10bf0`).
/// `$(VAR)` / `${VAR}` refs are expanded (PVA-466 parity). Returns `None`
/// when neither variable is set.
pub fn server_tls_keychain() -> Option<OsString> {
    env::get("EPICS_PVAS_TLS_KEYCHAIN")
        .or_else(|| env::get("EPICS_PVA_TLS_KEYCHAIN"))
        .map(expand_dollar_vars)
}

/// TLS option string for a server-side endpoint.
///
/// Reads `EPICS_PVAS_TLS_OPTIONS`, falling back to `EPICS_PVA_TLS_OPTIONS`
/// when the server-specific form is unset — matching pvxs `Config::server()`
/// `pickone({"EPICS_PVAS_TLS_OPTIONS", "EPICS_PVA_TLS_OPTIONS"})`
/// (`config.cpp:483`, `tls` `b3a10bf0`), which takes the first variable
/// present and ignores the other (no merge). An operator who sets only the
/// server form (e.g. `EPICS_PVAS_TLS_OPTIONS=client_cert=require`) must see
/// it honoured; reading the shared `EPICS_PVA_TLS_OPTIONS` alone silently
/// dropped the requirement and let the server accept certless clients
/// (fail-open). Options are not `$(VAR)`-expanded (pvxs parity; the string is
/// option tokens like `client_cert=require`, not a path). Empty when neither
/// set.
pub fn server_tls_options() -> String {
    getenv("EPICS_PVAS_TLS_OPTIONS")
        .or_else(|| getenv("EPICS_PVA_TLS_OPTIONS"))
        .unwrap_or_default()
}

/// Presence-aware `disable_plaintext` flag parsed from the server TLS
/// options (`EPICS_PVAS_TLS_OPTIONS` → `EPICS_PVA_TLS_OPTIONS`, see
/// [`server_tls_options`]).
///
/// Mirrors `parseTLSOptions` (`config.cpp:435-465` on the personal fork
/// branch `fork/fix/tls-peer-identity-and-downgrade` `6547f25`, the only tree
/// that has `disable_plaintext` at all): the option string is
/// whitespace-split into `key=value` tokens (`split_into`,
/// `config.cpp:224-239`, `tls` `b3a10bf0`) and the flag is set ONLY for the
/// exact value `true` / `false`; an unknown value is ignored and a later
/// token wins over an earlier one. Returns `None` when no
/// `disable_plaintext=` token is present, so
/// [`crate::server_native::PvaServerConfig::with_env`] preserves a
/// caller-supplied value rather than forcing it to the default — the same
/// `PickOne`-style presence rule the port helpers use.
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
/// keychain spec (`ossl.cpp:223-229` on pvxs `tls` `b3a10bf0`, see
/// [`split_keychain_spec`]), not from a dedicated env var. This Rust-only
/// convenience is consulted
/// ONLY when the keychain spec carries no inline `;` suffix. Reads
/// `EPICS_PVA_TLS_KEYCHAIN_PASSWORD`; `$(VAR)` / `${VAR}` refs are
/// expanded. Returns `None` when unset.
pub fn client_tls_keychain_password() -> Option<OsString> {
    env::get("EPICS_PVA_TLS_KEYCHAIN_PASSWORD").map(expand_dollar_vars)
}

/// Split a TLS keychain spec into `(path, inline_password)` the way pvxs
/// `ossl.cpp:223-229` does: text before the FIRST `;` is the keychain
/// path, text after it (when a `;` is present) is the PKCS#12 password.
///
/// The `ossl.cpp` citations here and in `crate::auth::tls` name pvxs `tls`
/// `b3a10bf0`: `src/ossl.cpp` is on no default-branch revision of pvxs, so the
/// crate's pin table — *no settled pin* for `pvxs` — cannot resolve them.
/// `tls` is the unmerged upstream x509 branch on `epics-base/pvxs`.
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
pub fn split_keychain_spec(spec: impl AsRef<OsStr>) -> (PathBuf, Option<OsString>) {
    let spec = spec.as_ref();
    let bytes = spec.as_encoded_bytes();
    match bytes.iter().position(|&b| b == b';') {
        Some(i) => {
            // SAFETY: both halves are cut from `spec` at an ASCII `;`.
            let (path, password) = unsafe {
                (
                    OsStr::from_encoded_bytes_unchecked(&bytes[..i]),
                    OsStr::from_encoded_bytes_unchecked(&bytes[i + 1..]),
                )
            };
            (path.into(), Some(password.to_owned()))
        }
        None => (spec.into(), None),
    }
}

/// Resolve a `EPICS_PVA*_INTF_ADDR_LIST` value to a deduplicated list of
/// bind addresses, each with its token's own port, 0 when it names none.
///
/// pvxs parses interface lists through the SAME `split_addr_into` /
/// `SockEndpoint` path as every other address list (config.cpp:151-183,
/// 418-419, 592-593), so a DNS hostname is resolved before it constrains
/// the bind/search interfaces: `SockEndpoint` calls `SockAddr::setAddress`
/// (util.cpp:523-540), which falls back to a synchronous DNS lookup,
/// IPv4-preferred. The earlier `parse::<IpAddr>()`-only split dropped any
/// hostname token, silently turning a constrained interface list into the
/// empty all-NIC default (client search/broadcast) or wildcard bind
/// (server listener) — a broader exposure than pvxs.
///
/// The list comes back in pvxs's `split_addr_into` order, `default_port` in the key
/// only: the port stays 0 in the entry itself, so the bind fills it from the
/// port the server ends up with, an explicit one included. A multicast entry keeps its `@iface`: the server expands it into
/// an interface of its own (`config.cpp:497-509`). An invalid entry is
/// dropped as [`parse_endpoints_with_port`] drops it; the environment's lists
/// fail or log as pvxs's do.
pub fn resolve_intf_addr_list(value: &str, default_port: u16) -> Vec<Endpoint> {
    split_order(parse_endpoints_with_port(value, 0), default_port)
}

/// pvxs `SockEndpoint`'s text (`config.cpp:125-135`): the modifiers print
/// for a multicast address only, and an unset TTL is pvxs's `-1`
/// (`osiSockExt.h:142`), so `pvxsr` shows a bare `224.0.2.3` as
/// `224.0.2.3,-1`; a TTL of 0 prints nothing.
pub(crate) fn sock_endpoint_text(ep: &Endpoint) -> String {
    let mut text = sock_addr_text(ep.addr);
    if ep.addr.ip().is_multicast() {
        if ep.ttl != 0 {
            text.push_str(&format!(",{}", ep.ttl));
        }
        if let Some(iface) = &ep.iface {
            text.push_str(&format!("@{iface}"));
        }
    }
    text
}

/// pvxs `SockAddr`'s text (`util.cpp:660-690`): `[v6]%scope`, and no port
/// when it is 0.
pub(crate) fn sock_addr_text(addr: SocketAddr) -> String {
    let mut text = match addr {
        SocketAddr::V4(a) => a.ip().to_string(),
        SocketAddr::V6(a) if a.scope_id() != 0 => format!("[{}]%{}", a.ip(), a.scope_id()),
        SocketAddr::V6(a) => format!("[{}]", a.ip()),
    };
    if addr.port() != 0 {
        text.push_str(&format!(":{}", addr.port()));
    }
    text
}

/// `EPICS_PVA_INTF_ADDR_LIST` as endpoints, multicast modifiers kept: the
/// client's `Config::interfaces`.
pub fn client_intf_endpoints() -> Vec<Endpoint> {
    getenv("EPICS_PVA_INTF_ADDR_LIST")
        .map(|s| split_addr_list("EPICS_PVA_INTF_ADDR_LIST", &s, 0))
        .unwrap_or_default()
}

/// Parse `EPICS_PVA_INTF_ADDR_LIST` — client-side interface bind list.
/// Empty = bind to 0.0.0.0 (default behaviour). DNS hostnames resolve
/// (IPv4-preferred) and `$(VAR)` / `${VAR}` refs expand; see
/// `resolve_intf_addr_list`.
pub fn list_intf_addresses() -> Vec<IpAddr> {
    let mut out: Vec<IpAddr> = Vec::new();
    for ip in client_intf_endpoints().iter().map(|ep| ep.addr.ip()) {
        if !out.contains(&ip) {
            out.push(ip);
        }
    }
    out
}

/// `EPICS_PVAS_INTF_ADDR_LIST`, the server's interfaces, a port-less entry
/// left at 0 for the bind to fill from `tcp_port`. `None` when unset; an
/// invalid entry fails it as pvxs's `required=true` does
/// (`config.cpp:418-419`). The server has no `EPICS_PVA_INTF_ADDR_LIST`
/// fallback: pvxs reads that one for the client only (`config.cpp:418` vs
/// `592`), and softIocPVX 1.5.3 with only `EPICS_PVA_INTF_ADDR_LIST=10.99.99.99`
/// serves on the wildcard.
pub fn server_intf_addr_list_opt() -> Result<Option<Vec<Endpoint>>, String> {
    // The server reads `EPICS_PVAS_SERVER_PORT` before the interface list
    // (`config.cpp:402-419`), so that port is in the strings it sorts.
    match getenv("EPICS_PVAS_INTF_ADDR_LIST") {
        Some(raw) => split_addr_list_required(&raw, pvas_server_port()).map(Some),
        None => Ok(None),
    }
}

/// [`server_intf_addr_list_opt`], empty when unset.
pub fn server_intf_addr_list() -> Result<Vec<Endpoint>, String> {
    Ok(server_intf_addr_list_opt()?.unwrap_or_default())
}

/// [`server_intf_addr_list_opt`] on a given value: a server interface list
/// from somewhere other than the environment (pva-gateway-rs's `interface`)
/// parses, sorts and fails exactly as the env list does. `default_port` goes
/// into the sort key only; the entries keep 0 so the server's resolved port
/// still reaches the bind.
pub fn resolve_server_intf_addr_list(
    raw: &str,
    default_port: u16,
) -> Result<Option<Vec<Endpoint>>, String> {
    let eps = split_addr_list_required(raw, default_port)?;
    Ok((!eps.is_empty()).then_some(eps))
}

/// `EPICS_PVAS_IGNORE_ADDR_LIST`, the peers whose connections (TCP) and
/// searches (UDP) the server drops, pvxs `Config::ignoreAddrs`: each entry an
/// IP and a port, 0 matching any port from that IP. `None` when unset; an
/// invalid entry fails it as pvxs's `required=true` does
/// (`config.cpp:422-423`).
pub fn server_ignore_addr_list_opt() -> Result<Option<Vec<(IpAddr, u16)>>, String> {
    let Some(raw) = getenv("EPICS_PVAS_IGNORE_ADDR_LIST") else {
        return Ok(None);
    };
    let eps = split_addr_list_required(&raw, 0)?;
    Ok(Some(
        eps.into_iter()
            .map(|e| (e.addr.ip(), e.addr.port()))
            .collect(),
    ))
}

/// [`server_ignore_addr_list_opt`], empty when unset.
pub fn server_ignore_addr_list() -> Result<Vec<(IpAddr, u16)>, String> {
    Ok(server_ignore_addr_list_opt()?.unwrap_or_default())
}

/// Parse `EPICS_PVAS_BEACON_ADDR_LIST` — explicit beacon destinations
/// (default port = `EPICS_PVAS_BROADCAST_PORT`). Falls back to empty
/// when unset (caller should auto-discover NIC broadcasts).
///
/// pvxs `config.cpp:426-432` falls back to
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
/// (`evhelper.cpp:556-592`). Same env source + fallback
/// (`EPICS_PVAS_BEACON_ADDR_LIST` → `EPICS_PVA_ADDR_LIST`, `config.cpp:426-432`).
pub fn server_beacon_endpoints_opt() -> Option<Vec<Endpoint>> {
    server_beacon_endpoints_on(server_broadcast_port())
}

/// [`server_beacon_endpoints_opt`] with a port-less entry taking
/// `default_port`, a configuration's own `udp_port` (`config.cpp:427`).
pub(crate) fn server_beacon_endpoints_on(default_port: u16) -> Option<Vec<Endpoint>> {
    let (name, raw) = ["EPICS_PVAS_BEACON_ADDR_LIST", "EPICS_PVA_ADDR_LIST"]
        .into_iter()
        .find_map(|name| getenv(name).map(|raw| (name, raw)))?;
    Some(split_addr_list(name, &raw, default_port))
}

/// Discover per-NIC IPv4 broadcast addresses. Used to fan SEARCH
/// requests / BEACONs across all subnets the host is attached to.
/// Skips loopback and interfaces without a broadcast address.
///
/// Enumerates through [`epics_base_rs::net::iface_v4`], so it exists on every
/// target. The eligibility and destination rules
/// are `IfaceV4::search_destination`'s, which are C's
/// (`osdNetIfAddrs.c:130-151`): up and non-loopback, the broadcast address
/// unless it is `0.0.0.0`, else a point-to-point peer. The `if-addrs` version
/// this replaces tested neither `IFF_UP` nor the point-to-point case, so a
/// configured-but-down NIC contributed a destination and a VPN tunnel
/// contributed none.
pub fn list_broadcast_addresses(port: u16) -> Vec<SocketAddr> {
    epics_base_rs::net::iface_v4::broadcast_addrs()
        .into_iter()
        .map(|ip| SocketAddr::new(IpAddr::V4(ip), port))
        .collect()
}

/// pvxs `expandAddrList` (`config.cpp:284-308`): C
/// `osiSockDiscoverBroadcastAddresses` matched to each interface — every
/// NIC's destination for a wildcard, `127.0.0.1` itself for `127.0.0.1`
/// (`osdNetIfAddrs.c:42-55`), else the destination of the NIC holding that
/// address. No limited broadcast. An empty list expands to nothing: pvxs
/// makes the unconfigured default `0.0.0.0` before it gets here
/// (`config.cpp:491-493`).
pub fn expand_addr_list(interfaces: &[Ipv4Addr], port: u16) -> Vec<SocketAddr> {
    let ifaces = epics_base_rs::net::iface_v4::enumerate().unwrap_or_default();
    let mut out = Vec::new();
    for &want in interfaces {
        if want == Ipv4Addr::LOCALHOST {
            out.push(want);
            continue;
        }
        out.extend(
            ifaces
                .iter()
                .filter(|i| want.is_unspecified() || i.ip == want)
                .filter_map(|i| i.search_destination()),
        );
    }
    out.into_iter()
        .map(|ip| SocketAddr::new(IpAddr::V4(ip), port))
        .collect()
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

/// Give every port-less entry the list's default port, as the strings
/// pvxs `split_addr_into` stores do (`config.cpp:167-168`). The server's
/// interface list keeps 0 for its bind, so its `Config` copy is filled here.
fn set_default_port(eps: &mut [Endpoint], port: u16) {
    for e in eps.iter_mut() {
        if e.addr.port() == 0 {
            e.addr.set_port(port);
        }
    }
}

/// Drop duplicate `(ip, port)` ignore entries, preserving first-seen
/// order (pvxs `removeDups(ignoreAddrs)`, `config.cpp:525`).
fn dedup_ignore_addrs(addrs: &mut Vec<(IpAddr, u16)>) {
    let mut seen = std::collections::HashSet::new();
    addrs.retain(|x| seen.insert(*x));
}

/// pvxs `expandAddrList` then `addGroups` (`config.cpp:284-336`): each v4
/// interface's broadcasts join `dest` with port 0 (`[::]` counts as
/// `0.0.0.0` on Linux), then every multicast entry of `dest` joins `ifaces`,
/// once per external interface when it names none.
fn auto_expand(ifaces: &mut Vec<Endpoint>, dest: &mut Vec<Endpoint>) {
    let v4: Vec<Ipv4Addr> = ifaces
        .iter()
        .filter_map(|e| match e.addr.ip() {
            IpAddr::V4(v4) => Some(v4),
            IpAddr::V6(v6) if cfg!(target_os = "linux") && v6.is_unspecified() => {
                Some(Ipv4Addr::UNSPECIFIED)
            }
            IpAddr::V6(_) => None,
        })
        .collect();
    dest.extend(expand_addr_list(&v4, 0).into_iter().map(Endpoint::from));
    let mut external: Option<Vec<String>> = None;
    for ep in dest.iter().filter(|ep| ep.addr.ip().is_multicast()) {
        if ep.iface.is_some() {
            ifaces.push(ep.clone());
        } else {
            let names = external.get_or_insert_with(|| super::iface::IfaceMap::current().names());
            for name in names.iter() {
                ifaces.push(Endpoint {
                    iface: Some(name.clone()),
                    ..ep.clone()
                });
            }
        }
    }
}

/// The effective PVA configuration value — a client+server union of
/// `pvxs::client::Config` and `pvxs::server::Config` (`config.cpp`),
/// seeded from the environment via [`Config::from_client_env`] /
/// [`Config::from_server_env`] and finalised by [`Config::expand_client`] /
/// [`Config::expand_server`].
///
/// `expand()` is the single owner that turns the *as-configured* value
/// into the *as-used* value: it normalises the protocol ports, fills the
/// wildcard interface, fans the auto-address / auto-beacon broadcasts
/// into the destination lists, gives each destination its effective
/// port, collapses duplicates, and clamps the idle timeout — mirroring
/// pvxs `Config::expand()` (`config.cpp:485-529` server, `:624-651`
/// client). The post-`expand()` value is what an effective-config
/// readback (`pvinfo -D`, `operator<<`) should render.
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
    /// clears the flag (pvxs `autoAddrList`, `config.cpp:640-644`).
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

impl Config {
    /// Seed a client-role config from the `EPICS_PVA_*` environment.
    /// Mirrors `client::Config::fromEnv` (`config.cpp:552-599`): each
    /// field comes from the matching env accessor, the address list is
    /// parsed against the UDP port, and the timeout is scaled. Call
    /// [`Self::expand_client`] before use.
    pub fn from_client_env() -> Self {
        // `_fromDefs`'s read order, which is the order its warnings print.
        let (udp_port, tcp_port) = client_ports_logged();
        let address_list = getenv("EPICS_PVA_ADDR_LIST")
            .map(|s| split_addr_list("EPICS_PVA_ADDR_LIST", &s, udp_port))
            .unwrap_or_default();
        let name_servers = getenv("EPICS_PVA_NAME_SERVERS")
            .map(|s| split_addr_list("EPICS_PVA_NAME_SERVERS", &s, tcp_port))
            .unwrap_or_default()
            .into_iter()
            .map(|e| e.addr)
            .collect();
        let auto_addr_list = auto_addr_list_enabled();
        let interfaces = client_intf_endpoints();
        let tcp_timeout = conn_timeout_secs() * TMO_SCALE;
        Self {
            udp_port,
            tcp_port,
            auto_addr_list,
            auto_beacon: false,
            interfaces,
            address_list,
            name_servers,
            beacon_destinations: Vec::new(),
            ignore_addrs: Vec::new(),
            tcp_timeout,
        }
    }

    /// [`Self::tcp_timeout`] through `enforceTimeout`, as `expand` leaves it.
    pub fn enforced_tcp_timeout(&self) -> std::time::Duration {
        let mut tmo = self.tcp_timeout;
        enforce_timeout(&mut tmo);
        std::time::Duration::from_secs_f64(tmo)
    }

    /// Seed a server-role config from the `EPICS_PVAS_*` environment
    /// (falling back to `EPICS_PVA_*`). Mirrors `server::Config::fromEnv`
    /// (`config.cpp:397-443`), failing with the `what()` of an invalid
    /// interface or ignore entry. Call [`Self::expand_server`] before use.
    pub fn from_server_env() -> Result<Self, String> {
        // pvxs's read order: the ports, then an invalid interface stops it
        // before the beacon list logs anything.
        let (mut tcp_port, mut udp_port) = (5075, 5076);
        server_port_logged(SERVER_PORT_VARS, &mut tcp_port);
        server_port_logged(SERVER_BROADCAST_PORT_VARS, &mut udp_port);
        let mut interfaces = server_intf_addr_list()?;
        set_default_port(&mut interfaces, tcp_port);
        let ignore_addrs = server_ignore_addr_list()?;
        Ok(Self {
            udp_port,
            tcp_port,
            auto_addr_list: false,
            auto_beacon: auto_beacon_addr_list_enabled(),
            interfaces,
            address_list: Vec::new(),
            name_servers: Vec::new(),
            beacon_destinations: server_beacon_endpoints_on(udp_port).unwrap_or_default(),
            ignore_addrs,
            tcp_timeout: conn_timeout_secs() * TMO_SCALE,
        })
    }

    /// pvxs `client::Config::expand()` (`config.cpp:624-651`). Idempotent:
    /// `auto_addr_list` is cleared once consumed, as pvxs does.
    pub fn expand_client(&mut self) {
        // pvxs throws on a zero UDP port; `from_client_env` has already
        // replaced one, so this only covers a hand-built value.
        if self.udp_port == 0 {
            self.udp_port = 5076;
        }
        if self.tcp_port == 0 {
            self.tcp_port = 5075;
        }
        if self.interfaces.is_empty() {
            self.interfaces.push(Endpoint::from(SocketAddr::new(
                IpAddr::V4(Ipv4Addr::UNSPECIFIED),
                0,
            )));
        }
        if self.auto_addr_list {
            auto_expand(&mut self.interfaces, &mut self.address_list);
            self.auto_addr_list = false;
        }
        // The client dedups its destinations, not its interfaces.
        self.address_list = dedup_endpoints(std::mem::take(&mut self.address_list));
        enforce_timeout(&mut self.tcp_timeout);
    }

    /// `EPICS_PVA_CONN_TMO` as pvxs `updateDefs` writes it back:
    /// `tcpTimeout/tmoScale` (`config.cpp:620`).
    pub fn conn_tmo(&self) -> f64 {
        self.tcp_timeout / TMO_SCALE
    }

    /// pvxs `server::Config::expand()` (`config.cpp:485-529`). Idempotent:
    /// `auto_beacon` is cleared once consumed, as pvxs does.
    ///
    /// Returns pvxs's `interfaces` text after `expand()`. A group whose
    /// `@iface` names no interface derives pvxs's empty `SockAddr`
    /// (`evhelper.cpp:889-896`), printed `<>` (`util.cpp:690`): it binds
    /// nothing and expands to no beacon, so it is in the text only, never
    /// in `self.interfaces`.
    pub fn expand_server(&mut self) -> Vec<String> {
        if self.interfaces.is_empty() {
            self.interfaces.push(Endpoint::from(SocketAddr::new(
                IpAddr::V4(Ipv4Addr::UNSPECIFIED),
                0,
            )));
        }
        // `None` is the empty `SockAddr`.
        let mut slots: Vec<Option<Endpoint>> = self.interfaces.drain(..).map(Some).collect();
        // A group interface also binds its interface's address, or the
        // wildcard of its family (`config.cpp:498-509`).
        let groups: Vec<Endpoint> = slots
            .iter()
            .flatten()
            .filter(|ep| ep.addr.ip().is_multicast())
            .cloned()
            .collect();
        if !groups.is_empty() {
            let map = super::iface::IfaceMap::current();
            for ep in groups {
                let ip = match &ep.iface {
                    Some(name) => map.address_of(name),
                    None if ep.addr.is_ipv6() => Some(IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED)),
                    None => Some(IpAddr::V4(Ipv4Addr::UNSPECIFIED)),
                };
                slots.push(ip.map(|ip| Endpoint::from(SocketAddr::new(ip, 0))));
            }
        }
        if self.auto_beacon {
            let mut ifaces: Vec<Endpoint> = slots.iter().flatten().cloned().collect();
            let before = ifaces.len();
            auto_expand(&mut ifaces, &mut self.beacon_destinations);
            slots.extend(ifaces.drain(before..).map(Some));
            self.auto_beacon = false;
        }
        // pvxs `removeDups` keys every empty address alike, so one `<>`
        // stays, after the distinct entries that precede its first sighting.
        let empty_at = slots.iter().position(Option::is_none);
        let kept: Vec<Endpoint> = slots.into_iter().flatten().collect();
        // No slot before the first `None` is empty, so it indexes `kept` too.
        let empty_at = empty_at.map(|at| dedup_endpoints(kept[..at].to_vec()).len());
        self.interfaces = dedup_endpoints(kept);
        let mut text: Vec<String> = self.interfaces.iter().map(sock_endpoint_text).collect();
        if let Some(at) = empty_at {
            text.insert(at, "<>".to_string());
        }
        self.beacon_destinations = dedup_endpoints(std::mem::take(&mut self.beacon_destinations));
        dedup_ignore_addrs(&mut self.ignore_addrs);
        enforce_timeout(&mut self.tcp_timeout);
        text
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
    /// (config.cpp:229-255, :468-472). A key present in the process
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
        assert_eq!(parse_port_env("70000"), Ok(4464));
        assert_eq!(parse_port_env("65536"), Ok(0));
        assert_eq!(parse_port_env(" 5076 "), Ok(5076));
        assert_eq!(parse_port_env("\t5075\n"), Ok(5075));
        assert_eq!(parse_port_env("5075"), Ok(5075));
        assert_eq!(parse_port_env("0"), Ok(0));
        // stoull base 0.
        assert_eq!(parse_port_env("0x13c4"), Ok(5060));
        assert_eq!(parse_port_env("010"), Ok(8));
        assert_eq!(
            parse_port_env("[::1"),
            Err("Invalid input : \"[::1\"".to_string())
        );
        assert_eq!(
            parse_port_env("5076x"),
            Err("Extraneous characters after integer: \"5076x\"".to_string())
        );
        assert!(parse_port_env("").is_err());
        assert!(parse_port_env("999999999999999999999999999").is_err());
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
        assert_eq!(plain.ttl, -1);
        assert_eq!(plain.iface, None);

        let ttl = Endpoint::parse("224.0.2.3,5", 5076).expect("addr,ttl");
        assert_eq!(ttl.addr, "224.0.2.3:5076".parse().unwrap());
        assert_eq!(ttl.ttl, 5);
        assert_eq!(ttl.iface, None);

        let iface = Endpoint::parse("224.0.2.3@eth0", 5076).expect("addr@iface");
        assert_eq!(iface.ttl, -1);
        assert_eq!(iface.iface.as_deref(), Some("eth0"));

        let both = Endpoint::parse("224.0.2.3,5@eth0", 5076).expect("addr,ttl@iface");
        assert_eq!(both.addr, "224.0.2.3:5076".parse().unwrap());
        assert_eq!(both.ttl, 5);
        assert_eq!(both.iface.as_deref(), Some("eth0"));

        // An explicit port in the address part survives the modifiers.
        let ported = Endpoint::parse("224.0.2.3:9999,5@eth0", 5076).expect("addr:port,ttl@iface");
        assert_eq!(ported.addr, "224.0.2.3:9999".parse().unwrap());
        assert_eq!(ported.ttl, 5);
        assert_eq!(ported.iface.as_deref(), Some("eth0"));
    }

    /// A malformed entry fails with the `what()` pvxs's `SockEndpoint` throws.
    #[test]
    fn endpoint_parse_rejects_malformed() {
        assert_eq!(
            Endpoint::parse("224.0.2.3@eth0,5", 5076),
            Err("\"224.0.2.3@eth0,5\" comma expected before @".to_string())
        );
        assert_eq!(
            Endpoint::parse("224.0.2.3,abc", 5076),
            Err("Invalid input : \"abc\"".to_string())
        );
        // An empty address goes to the resolver, as `setAddress("")` does.
        for token in [",5", "@eth0"] {
            let err = Endpoint::parse(token, 5076).unwrap_err();
            assert!(err.starts_with("Error resolving \"\" : "), "{token}: {err}");
        }
    }

    /// pvxs reads the TTL with `parseTo<int64_t>` into an `int`: a negative,
    /// hex or out-of-`u8` TTL is accepted and kept, and its own default `-1`
    /// prints and re-parses.
    #[test]
    fn endpoint_ttl_is_pvxs_int() {
        for (token, ttl) in [
            ("224.0.2.3,-1", -1),
            ("224.0.2.3,-5", -5),
            ("224.0.2.3,0x10", 16),
            ("224.0.2.3,300", 300),
            ("224.0.2.3,4294967297", 1),
        ] {
            let ep = Endpoint::parse(token, 5076).expect(token);
            assert_eq!(ep.ttl, ttl, "{token}");
        }
        let ep = Endpoint::parse("224.0.2.3", 5076).unwrap();
        assert_eq!(sock_endpoint_text(&ep), "224.0.2.3:5076,-1");
        assert_eq!(ep.mcast_ttl(), 1);
        assert_eq!(Endpoint::parse("224.0.2.3,-1", 5076), Ok(ep));
    }

    /// A programmatic `SocketAddr` carries no multicast modifiers, so the
    /// `From` shim that lets `Vec<SocketAddr>` callers feed the endpoint-typed
    /// beacon list must yield a modifier-less endpoint.
    #[test]
    fn endpoint_from_socket_addr_has_no_modifiers() {
        let sa: SocketAddr = "224.0.2.3:5076".parse().unwrap();
        let ep: Endpoint = sa.into();
        assert_eq!(ep.addr, sa);
        assert_eq!(ep.ttl, -1);
        assert_eq!(ep.iface, None);
    }

    #[test]
    fn a_bracketed_v6_address_takes_the_default_port() {
        let got = parse_endpoints_with_port("[::] [::1]:7 [ff02::42:1],1@lo", 5076);
        let addrs: Vec<String> = got.iter().map(|e| e.addr.to_string()).collect();
        assert_eq!(addrs, ["[::]:5076", "[::1]:7", "[ff02::42:1]:5076"]);
        assert_eq!((got[2].ttl, got[2].iface.as_deref()), (1, Some("lo")));
    }

    /// A caller's invalid entry is skipped with pvxs `parseAddresses`'s
    /// warning (`config.cpp:263-265`), the valid ones kept.
    #[test]
    fn a_callers_invalid_entry_warns_as_parse_addresses() {
        use epics_base_rs::runtime::log;
        let heard = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let sink = heard.clone();
        let id = log::errlog_add_listener(move |m| {
            if let Some((_time, line)) = m.split_once(' ') {
                sink.lock().unwrap().push_str(line);
            }
        });
        let eps = parse_endpoints_with_port("10.0.0.1 10.0.0.2:x", 5076);
        log::errlog_flush();
        log::errlog_remove_listener(id);
        assert_eq!(eps.len(), 1);
        let heard = heard.lock().unwrap().clone();
        assert!(
            heard.starts_with("WARN pvxs.config Ignoring 10.0.0.2:x : "),
            "{heard:?}"
        );
    }

    /// The list splits on WHITESPACE only (pvxs `split_addr_into`); a comma
    /// is endpoint syntax, so it never multiplies the entry count.
    #[test]
    fn parse_endpoints_whitespace_only_split() {
        let eps = parse_endpoints_with_port("224.0.2.3,5@eth0 10.0.0.1 192.168.0.1:9", 5076);
        assert_eq!(eps.len(), 3, "three whitespace-separated endpoints");
        assert_eq!(eps[0].addr, "224.0.2.3:5076".parse().unwrap());
        assert_eq!(eps[0].ttl, 5);
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
        assert_eq!(deduped[0].ttl, 8, "longest TTL survives the combine");
    }

    #[test]
    fn dedup_beacon_endpoints_explicit_ttl_beats_default() {
        // `None` (OS default) TTL ranks below any explicit TTL via `Option`
        // ordering — a specified TTL always wins regardless of token order.
        let default_first =
            dedup_endpoints(parse_endpoints_with_port("224.0.2.3 224.0.2.3,4", 5076));
        assert_eq!(default_first.len(), 1);
        assert_eq!(default_first[0].ttl, 4);
        let explicit_first =
            dedup_endpoints(parse_endpoints_with_port("224.0.2.3,4 224.0.2.3", 5076));
        assert_eq!(explicit_first.len(), 1);
        assert_eq!(explicit_first[0].ttl, 4);
    }

    #[test]
    fn dedup_beacon_endpoints_keeps_distinct_iface() {
        // Same multicast group on two different interfaces is two distinct
        // destinations (pvxs keys dedup by `(addr, iface)`, not addr alone).
        let eps = parse_endpoints_with_port("224.0.2.3,8@eth8 224.0.2.3,8@eth9", 5076);
        let deduped = dedup_endpoints(eps);
        assert_eq!(deduped.len(), 2, "distinct @iface stays separate");
        assert_eq!(deduped[0].iface.as_deref(), Some("eth8"));
        assert_eq!(deduped[1].iface.as_deref(), Some("eth9"));
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
        assert_eq!(deduped[1].ttl, 8, "merged TTL on the first-seen slot");
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
    fn list_broadcast_addresses_has_no_limited_broadcast() {
        // `osiSockDiscoverBroadcastAddresses` names NICs, never 255.255.255.255.
        let bcasts = list_broadcast_addresses(5076);
        assert!(
            !bcasts
                .iter()
                .any(|a| a.ip() == IpAddr::V4(Ipv4Addr::BROADCAST))
        );
    }

    #[test]
    fn expand_addr_list_is_pvxs_expand_addr_list() {
        let port = 5076;
        assert_eq!(
            expand_addr_list(&[Ipv4Addr::LOCALHOST], port),
            vec![SocketAddr::from((Ipv4Addr::LOCALHOST, port))]
        );
        assert!(expand_addr_list(&[Ipv4Addr::new(192, 0, 2, 1)], port).is_empty());
        assert!(expand_addr_list(&[], port).is_empty());
        let all = expand_addr_list(&[Ipv4Addr::UNSPECIFIED], port);
        assert!(
            all.iter()
                .all(|a| a.ip() != IpAddr::V4(Ipv4Addr::BROADCAST) && a.port() == port)
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
        let server = server_intf_addr_list().unwrap();
        let ignore = server_ignore_addr_list().unwrap();
        assert_eq!(
            client,
            vec![IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1))],
            "EPICS_PVA_INTF_ADDR_LIST $(VAR) must expand"
        );
        assert_eq!(
            server,
            vec![Endpoint::from(SocketAddr::new(
                Ipv4Addr::LOCALHOST.into(),
                0
            ))],
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
        let server = server_intf_addr_list().unwrap();
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
            vec![Endpoint::from(SocketAddr::new(
                Ipv4Addr::LOCALHOST.into(),
                0
            ))],
            "EPICS_PVAS_INTF_ADDR_LIST=localhost must resolve to 127.0.0.1, not drop to []"
        );
    }

    /// The server list is `EPICS_PVAS_INTF_ADDR_LIST` alone: softIocPVX
    /// 1.5.3 with only `EPICS_PVA_INTF_ADDR_LIST=10.99.99.99` listens on
    /// `*:5291` with no bind error, and `pvxsr` shows
    /// `EPICS_PVAS_INTF_ADDR_LIST=0.0.0.0`.
    /// pvxs parses interface tokens as `SockEndpoint`s (`config.cpp:32-61`),
    /// so a `,ttl@iface` token is an interface, modifiers kept, not an
    /// unresolvable name.
    #[test]
    fn intf_list_keeps_multicast_modifier_tokens() {
        assert_eq!(
            resolve_intf_addr_list("224.0.2.3,5@lo 10.0.0.1", 0),
            vec![
                Endpoint::parse("10.0.0.1", 0).unwrap(),
                Endpoint::parse("224.0.2.3,5@lo", 0).unwrap(),
            ]
        );
    }

    /// The sort key is pvxs's `SockEndpoint` text: softIocPVX 1.5.3 with
    /// `EPICS_PVAS_INTF_ADDR_LIST="224.0.2.30 224.0.2.3"` shows
    /// `224.0.2.3,-1 224.0.2.30,-1 0.0.0.0` in `pvxsr`.
    #[test]
    fn sock_endpoint_text_prints_pvxs_multicast_modifiers() {
        let text = |t| sock_endpoint_text(&Endpoint::parse(t, 0).unwrap());
        assert_eq!(text("224.0.2.3"), "224.0.2.3,-1");
        assert_eq!(text("224.0.2.3,5@lo"), "224.0.2.3,5@lo");
        assert_eq!(text("224.0.2.3,0"), "224.0.2.3");
        assert_eq!(text("10.0.0.1,5@lo"), "10.0.0.1");
    }

    #[test]
    #[serial_test::serial(epics_env)]
    fn server_intf_list_ignores_the_client_variable() {
        unsafe {
            std::env::remove_var("EPICS_PVAS_INTF_ADDR_LIST");
            std::env::set_var("EPICS_PVA_INTF_ADDR_LIST", "10.99.99.99");
        }
        let got = (
            server_intf_addr_list_opt(),
            server_intf_addr_list(),
            list_intf_addresses(),
        );
        unsafe { std::env::remove_var("EPICS_PVA_INTF_ADDR_LIST") };
        let client = IpAddr::V4(Ipv4Addr::new(10, 99, 99, 99));
        assert_eq!(got, (Ok(None), Ok(Vec::new()), vec![client]));
    }

    /// Every list comes back sorted and deduped by its printed text, the
    /// default port filled in, as `split_addr_into` leaves it; a required
    /// list keeps an entry's absent port 0.
    #[test]
    fn split_lists_sort_and_dedup_by_printed_text() {
        let addrs = |eps: Vec<Endpoint>| eps.iter().map(|e| e.addr.to_string()).collect::<Vec<_>>();
        // "10.0.0.10:5076" < "10.0.0.1:5076": ':' sorts after '0'.
        let got = split_addr_list("T", "10.0.0.2 10.0.0.1 10.0.0.10 10.0.0.1:5076", 5076);
        assert_eq!(
            addrs(got),
            ["10.0.0.10:5076", "10.0.0.1:5076", "10.0.0.2:5076"]
        );
        let got = split_addr_list_required("10.0.0.2 10.0.0.1 10.0.0.10 10.0.0.1:5075", 5075);
        assert_eq!(
            addrs(got.unwrap()),
            ["10.0.0.10:0", "10.0.0.1:0", "10.0.0.2:0"]
        );
    }

    /// The server's interface and ignore lists are pvxs `required=true`
    /// lists (`config.cpp:418-424`): unset or blank leaves the caller's value,
    /// and one invalid entry fails the list, the valid ones beside it
    /// included, where dropping it would bind or filter less than was asked.
    #[test]
    #[serial_test::serial(epics_env)]
    fn server_required_lists_fail_on_any_invalid_entry() {
        type Read = fn() -> Result<Option<usize>, String>;
        let intf: Read = || server_intf_addr_list_opt().map(|v| v.map(|v| v.len()));
        let ignore: Read = || server_ignore_addr_list_opt().map(|v| v.map(|v| v.len()));
        for (var, read) in [
            ("EPICS_PVAS_INTF_ADDR_LIST", intf),
            ("EPICS_PVAS_IGNORE_ADDR_LIST", ignore),
        ] {
            let with = |value: Option<&str>| {
                // SAFETY: serialised by `epics_env`.
                unsafe {
                    match value {
                        Some(v) => std::env::set_var(var, v),
                        None => std::env::remove_var(var),
                    }
                }
                let got = read();
                unsafe { std::env::remove_var(var) };
                got
            };
            assert_eq!(with(None), Ok(None), "{var}");
            assert_eq!(with(Some("   ")), Ok(Some(0)), "{var}");
            assert_eq!(with(Some("127.0.0.1 localhost:7")), Ok(Some(2)), "{var}");
            assert_eq!(
                with(Some("127.0.0.1 127.0.0.2@lo,5")),
                Err(
                    "invalid endpoint \"127.0.0.2@lo,5\" \"127.0.0.2@lo,5\" comma \
                     expected before @"
                        .to_string()
                ),
                "{var}"
            );
            // RFC 6761: `.invalid` never resolves.
            let err = with(Some("nosuchhost.invalid 127.0.0.1")).unwrap_err();
            assert!(
                err.starts_with(
                    "invalid endpoint \"nosuchhost.invalid\" Error resolving \
                     \"nosuchhost.invalid\" : "
                ),
                "{var}: {err}"
            );
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

    /// PVA-40: an ignore entry keeps pvxs port semantics — a bare IP or host
    /// is a wildcard (port 0 matches any port from that IP), `ip:port` and
    /// `host:port` keep theirs — and a hostname resolves.
    #[test]
    #[serial_test::serial(epics_env)]
    fn server_ignore_addr_list_resolves_hostname_tokens() {
        // SAFETY: serialised by `epics_env`.
        unsafe {
            std::env::set_var(
                "EPICS_PVAS_IGNORE_ADDR_LIST",
                "localhost localhost:5076 10.0.0.1:5075 192.168.1.1",
            );
        }
        let list = server_ignore_addr_list().unwrap();
        unsafe {
            std::env::remove_var("EPICS_PVAS_IGNORE_ADDR_LIST");
        }
        // In `split_order`: "10.0.0.1:5075" < "127.0.0.1" < "127.0.0.1:5076".
        let ports: Vec<u16> = list.iter().map(|(_, port)| *port).collect();
        assert_eq!(ports, [5075, 0, 5076, 0], "{list:?}");
        assert_eq!(list[0].0, IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)));
        assert!(
            list[1].0.is_loopback() && list[2].0.is_loopback(),
            "{list:?}"
        );
        assert_eq!(list[3].0, IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1)));
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
        }
        assert_eq!(max_connections(), 1024);
    }

    /// server TLS options resolve PVAS-first, then shared PVA (pvxs
    /// `pickone({EPICS_PVAS_TLS_OPTIONS, EPICS_PVA_TLS_OPTIONS})`,
    /// config.cpp:483, `tls` `b3a10bf0`) — first present wins, no merge.
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

    /// pvxs `pickone` takes the first variable `getenv` returns, bytes and
    /// all: a PVAS value that is not UTF-8 still wins over the PVA one.
    #[test]
    #[cfg(unix)]
    #[serial_test::serial(epics_env)]
    fn server_tls_options_takes_a_pvas_value_that_is_not_utf8() {
        use std::os::unix::ffi::OsStrExt;
        let prev_pvas = std::env::var_os("EPICS_PVAS_TLS_OPTIONS");
        let prev_pva = std::env::var_os("EPICS_PVA_TLS_OPTIONS");
        unsafe {
            std::env::set_var(
                "EPICS_PVAS_TLS_OPTIONS",
                std::ffi::OsStr::from_bytes(b"client_cert=require \xff"),
            );
            std::env::set_var("EPICS_PVA_TLS_OPTIONS", "client_cert=optional");
        }
        assert_eq!(server_tls_options(), "client_cert=require \u{FFFD}");
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

    /// `disable_plaintext` is parsed out of the server TLS options the way
    /// `parseTLSOptions` does (config.cpp:453-460 on the fork branch
    /// `fork/fix/tls-peer-identity-and-downgrade` `6547f25`): `true` /
    /// `false` set the flag, an unknown value is ignored, and an absent key
    /// yields `None` so `with_env` preserves the caller value.
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
    /// config.cpp:479, `tls` `b3a10bf0`). The client path stays PVA-only
    /// (config.cpp:654).
    #[test]
    #[serial_test::serial(epics_env)]
    fn server_tls_keychain_prefers_pvas_then_pva() {
        let prev_pvas = std::env::var("EPICS_PVAS_TLS_KEYCHAIN").ok();
        let prev_pva = std::env::var("EPICS_PVA_TLS_KEYCHAIN").ok();
        unsafe {
            std::env::set_var("EPICS_PVAS_TLS_KEYCHAIN", "/srv/server.p12");
            std::env::set_var("EPICS_PVA_TLS_KEYCHAIN", "/cli/client.p12");
        }
        assert_eq!(
            server_tls_keychain().as_deref(),
            Some(OsStr::new("/srv/server.p12"))
        );
        unsafe {
            std::env::remove_var("EPICS_PVAS_TLS_KEYCHAIN");
        }
        assert_eq!(
            server_tls_keychain().as_deref(),
            Some(OsStr::new("/cli/client.p12"))
        );
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

    /// `split_keychain_spec` matches pvxs `ossl.cpp:223-229` (`tls`
    /// `b3a10bf0`): split at the FIRST `;`, path before, password after;
    /// no `;` → no inline password;
    /// trailing `;` → explicit empty password. One case per boundary.
    #[test]
    fn split_keychain_spec_matches_pvxs_first_semicolon() {
        fn split_text(spec: &str) -> (String, Option<String>) {
            let (path, password) = split_keychain_spec(spec);
            (
                path.into_os_string().into_string().unwrap(),
                password.map(|p| p.into_string().unwrap()),
            )
        }
        // No `;`: whole spec is the path, no inline password.
        assert_eq!(split_text("cert.p12"), ("cert.p12".to_string(), None));
        // `;secret`: path + non-empty password.
        assert_eq!(
            split_text("cert.p12;secret"),
            ("cert.p12".to_string(), Some("secret".to_string()))
        );
        // Trailing `;`: explicit empty password (distinct from None).
        assert_eq!(
            split_text("cert.p12;"),
            ("cert.p12".to_string(), Some(String::new()))
        );
        // Multiple `;`: split at the FIRST only; the rest is the password
        // verbatim (pvxs find_first_of + substr(sep+1)).
        assert_eq!(
            split_text("cert.p12;a;b"),
            ("cert.p12".to_string(), Some("a;b".to_string()))
        );
        // Leading `;`: empty path, password is the remainder (pvxs would
        // then fail to open the empty path — we preserve the same split).
        assert_eq!(
            split_text(";onlypw"),
            (String::new(), Some("onlypw".to_string()))
        );
    }

    /// The keychain spec is `getenv`'s bytes: a non-UTF-8 path and password
    /// reach the loader unchanged, `$(VAR)` values included.
    #[cfg(unix)]
    #[test]
    #[serial_test::serial(epics_env)]
    fn a_non_utf8_keychain_spec_keeps_its_bytes() {
        use std::os::unix::ffi::OsStrExt;
        let key = "EPICS_RS_TEST_KEYCHAIN_DIR";
        // SAFETY: single-threaded under the `epics_env` serial group.
        unsafe { std::env::set_var(key, OsStr::from_bytes(b"/d\xff")) };
        let spec = expand_dollar_vars(OsStr::from_bytes(
            b"$(EPICS_RS_TEST_KEYCHAIN_DIR)/k\xfe.p12;p\xfdw",
        ));
        // SAFETY: same serial group.
        unsafe { std::env::remove_var(key) };
        let (path, password) = split_keychain_spec(&spec);
        assert_eq!(path.as_os_str().as_bytes(), b"/d\xff/k\xfe.p12");
        assert_eq!(
            password.as_deref().map(OsStr::as_bytes),
            Some(&b"p\xfdw"[..])
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
    /// config.cpp:631-632). An explicit non-zero value and the unset
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
        let bare = Config::from_client_env().name_servers;
        assert_eq!(
            bare.iter().map(SocketAddr::port).collect::<Vec<_>>(),
            vec![5075],
            "bare name-server token must use the expanded default port, not 0"
        );

        unsafe {
            std::env::set_var("EPICS_PVA_NAME_SERVERS", "127.0.0.1:9876");
        }
        let explicit = Config::from_client_env().name_servers;
        assert_eq!(
            explicit.iter().map(SocketAddr::port).collect::<Vec<_>>(),
            vec![9876],
            "an explicit name-server port is preserved verbatim"
        );

        unsafe {
            std::env::remove_var("EPICS_PVA_NAME_SERVERS");
        }
        assert!(
            Config::from_client_env().name_servers.is_empty(),
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
            Config::from_client_env()
                .name_servers
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
            Config::from_client_env()
                .name_servers
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
            Config::from_client_env()
                .name_servers
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
        c.expand_client();
        assert_eq!(c.udp_port, 5076);
        assert_eq!(c.tcp_port, 5075);
    }

    /// An empty interface list expands to the wildcard `0.0.0.0` — pvxs
    /// "no addresses isn't interesting" (`config.cpp:492-494, 637-638`).
    #[test]
    fn config_expand_fills_wildcard_interface() {
        let mut c = bare_config();
        c.expand_client();
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
    /// (`config.cpp:640-644`). The flag is idempotent: a second `expand()`
    /// adds nothing.
    #[test]
    fn config_expand_consumes_auto_addr_list() {
        let mut c = bare_config();
        c.auto_addr_list = true;
        c.expand_client();
        assert!(
            !c.auto_addr_list,
            "auto_addr_list must be cleared after expand"
        );
        // pvxs `expandAddrList` adds them with port 0 (`config.cpp:304`).
        let want: Vec<SocketAddr> = expand_addr_list(&[Ipv4Addr::UNSPECIFIED], 0);
        let got: Vec<SocketAddr> = c.address_list.iter().map(|e| e.addr).collect();
        assert_eq!(
            got, want,
            "auto expansion is expandAddrList over the wildcard"
        );
        let n = c.address_list.len();
        c.expand_client();
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
                ttl: 1,
                iface: None,
            },
            Endpoint {
                addr: mcast,
                ttl: 8,
                iface: None,
            },
        ];
        c.expand_client();
        assert_eq!(
            c.address_list.len(),
            1,
            "duplicate (addr,iface) must collapse"
        );
        assert_eq!(c.address_list[0].ttl, 8, "longest TTL must win");
    }

    /// An auto-expanded broadcast keeps port 0, so it is not a duplicate of
    /// the same address given with the UDP port (`config.cpp:304, 647`).
    #[test]
    fn config_expand_keeps_auto_broadcasts_portless() {
        let mut c = bare_config();
        c.auto_addr_list = true;
        let Some(first) = expand_addr_list(&[Ipv4Addr::UNSPECIFIED], 0)
            .first()
            .copied()
        else {
            return;
        };
        let mut explicit = first;
        explicit.set_port(5076);
        c.address_list = vec![Endpoint::from(explicit)];
        c.expand_client();
        let got: Vec<SocketAddr> = c.address_list.iter().map(|e| e.addr).collect();
        assert!(got.contains(&explicit) && got.contains(&first), "{got:?}");
    }

    /// The server's group interface also binds the wildcard of its family,
    /// and its auto beacons clear the flag (`config.cpp:498-519`).
    #[test]
    fn config_expand_server_adds_group_wildcard() {
        let mut c = bare_config();
        let group = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(224, 0, 2, 3)), 0);
        c.interfaces = vec![Endpoint::from(group)];
        c.auto_beacon = true;
        c.expand_server();
        assert!(!c.auto_beacon);
        let got: Vec<SocketAddr> = c.interfaces.iter().map(|e| e.addr).collect();
        assert_eq!(
            got,
            vec![group, SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0)]
        );
    }

    /// The idle timeout is clamped to the pvxs `enforceTimeout` bounds
    /// (`config.cpp:373-391`): non-positive → 40 s default, below the 2 s
    /// floor → 2 s, a sane value is preserved.
    #[test]
    fn config_expand_enforces_timeout_bounds() {
        let mut zero = bare_config();
        zero.tcp_timeout = 0.0;
        zero.expand_client();
        assert_eq!(zero.tcp_timeout, 40.0);

        let mut tiny = bare_config();
        tiny.tcp_timeout = 1.0;
        tiny.expand_client();
        assert_eq!(tiny.tcp_timeout, 2.0);

        let mut sane = bare_config();
        sane.tcp_timeout = 40.0;
        sane.expand_client();
        assert_eq!(sane.tcp_timeout, 40.0);

        let mut nan = bare_config();
        nan.tcp_timeout = f64::NAN;
        nan.expand_client();
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
