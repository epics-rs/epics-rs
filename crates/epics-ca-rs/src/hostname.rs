//! Peer address → the text libca shows for it.
//!
//! Everywhere a CA client names the server it is talking to — `cainfo`'s
//! `Host:` line (`ca_host_name`), the `host=%s ctx=%s` an exception block
//! carries — libca prints a REVERSE-RESOLVED host name, not the dotted IP.
//! The port printed the dotted IP at every one of those sites (W10-B5), so
//! the divergence is not one line in `cainfo`; it is "the port has no
//! `ipAddrToA`". This module is that missing function, and the single owner
//! of the address → text direction.
//!
//! Two entry points, because libca has two:
//!
//! * [`ip_addr_to_a`] is libcom's `ipAddrToA` (`osiSock.c:92-114`) — a
//!   synchronous `getnameinfo` that BLOCKS on the resolver. Correct for a
//!   one-shot tool, which is exactly what C's `ca_host_name` gives a
//!   connected `cainfo`; must never run on a task that other channels'
//!   progress depends on, since an IP with no PTR record stalls for the
//!   resolver's full timeout.
//! * `cached_name` is libca's `hostNameCache` + the process-wide
//!   `ipAddrToAsciiEngine` singleton (`hostNameCache.cpp`,
//!   `ipAddrToAsciiEngine.cpp`) — it NEVER blocks: it answers from the cache,
//!   and on a miss it answers with the dotted IP and resolves in the
//!   background so the next caller has the name. libca's own asynchronous
//!   paths (`cac::defaultExcep` → `tcpiiu::getHostName`) print exactly that:
//!   the dotted IP until the engine has answered, the name afterwards.

// RTEMS-EXEC-MODEL-ALLOW(1): the flavored test asserts resolution inside a
// multi-thread tokio runtime. These run and pass in the
// feature-ON suite on the tokio driver.
use std::net::SocketAddr;
use std::sync::OnceLock;

use dashmap::DashMap;

/// libcom `ipAddrToA` (`osiSock.c:92-114`): reverse-resolve the address and
/// return `<name>:<port>`; on ANY resolver failure fall back to
/// `ipAddrToDottedIP`'s `<a.b.c.d>:<port>`, which is what `SocketAddr`'s
/// `Display` already writes.
///
/// The lookup is libcom's `ipAddrToHostName` (`os/posix/osdSock.c:229-248`):
/// `getnameinfo` with `NI_NAMEREQD`, so an address with no PTR record is a
/// failure rather than a stringified IP — that flag is what makes C's
/// fallback branch reachable at all.
///
/// BLOCKING. See the module docs before calling this from a task.
pub fn ip_addr_to_a(addr: SocketAddr) -> String {
    match ip_addr_to_host_name(addr) {
        Some(name) => format!("{name}:{}", addr.port()),
        None => addr.to_string(),
    }
}

/// libcom `ipAddrToHostName`. `None` is C's `return 0` — "no name", which is
/// the only thing the caller is allowed to distinguish.
///
/// The reverse lookup itself is the OS `getnameinfo(NI_NAMEREQD)` — the same
/// call C libcom makes. It lives behind [`resolve_ptr`] because that symbol is
/// in `libc` on unix but in Winsock on Windows.
fn ip_addr_to_host_name(addr: SocketAddr) -> Option<String> {
    // `socket2` owns the `SocketAddr` → `sockaddr` conversion (and its
    // length), so this module does not hand-roll a `sockaddr_in`.
    let sa = socket2::SockAddr::from(addr);
    resolve_ptr(&sa)
}

/// POSIX `getnameinfo(NI_NAMEREQD)` — the resolver C libcom uses on unix.
#[cfg(unix)]
fn resolve_ptr(sa: &socket2::SockAddr) -> Option<String> {
    let mut buf = [0 as libc::c_char; 256];
    // SAFETY: `sa` outlives the call and reports its own length; `buf` is
    // `buf.len()` bytes of writable storage. `getnameinfo` NUL-terminates
    // within that bound or returns non-zero.
    let rc = unsafe {
        libc::getnameinfo(
            sa.as_ptr(),
            sa.len(),
            buf.as_mut_ptr(),
            buf.len() as libc::socklen_t,
            std::ptr::null_mut(),
            0,
            libc::NI_NAMEREQD,
        )
    };
    if rc != 0 {
        return None;
    }
    // SAFETY: `getnameinfo` returned 0, so `buf` holds a NUL-terminated name.
    let name = unsafe { std::ffi::CStr::from_ptr(buf.as_ptr()) };
    let name = name.to_str().ok()?;
    (!name.is_empty()).then(|| name.to_string())
}

/// Winsock `getnameinfo(NI_NAMEREQD)` — the same resolver C EPICS uses on
/// Windows, where the symbol lives in `ws2_32` rather than the CRT. Winsock is
/// already initialised: no socket in this process exists without it.
#[cfg(windows)]
fn resolve_ptr(sa: &socket2::SockAddr) -> Option<String> {
    use windows_sys::Win32::Networking::WinSock::{NI_NAMEREQD, SOCKADDR, getnameinfo};
    let mut buf = [0u8; 256];
    // SAFETY: `sa` outlives the call and reports its own length; `buf` is
    // `buf.len()` bytes of writable storage. The `SockAddr` is a C `sockaddr`
    // by layout, so the cross-crate pointer cast is a reinterpret of the same
    // bytes Winsock expects. `getnameinfo` NUL-terminates within the bound or
    // returns non-zero.
    let rc = unsafe {
        getnameinfo(
            sa.as_ptr() as *const SOCKADDR,
            sa.len() as i32,
            buf.as_mut_ptr(),
            buf.len() as u32,
            std::ptr::null_mut(),
            0,
            NI_NAMEREQD as i32,
        )
    };
    if rc != 0 {
        return None;
    }
    // SAFETY: `getnameinfo` returned 0, so `buf` holds a NUL-terminated name.
    let name = unsafe { std::ffi::CStr::from_ptr(buf.as_ptr() as *const libc::c_char) };
    let name = name.to_str().ok()?;
    (!name.is_empty()).then(|| name.to_string())
}

/// The resolved names libca's `ipAddrToAsciiEngine` has answered so far.
/// Process-wide, as it is in libca — the engine is a singleton shared by
/// every `hostNameCache` in the process.
fn cache() -> &'static DashMap<SocketAddr, String> {
    static CACHE: OnceLock<DashMap<SocketAddr, String>> = OnceLock::new();
    CACHE.get_or_init(DashMap::new)
}

/// libca `hostNameCache::getName` — the NON-blocking half of this module.
///
/// Returns the resolved `<name>:<port>` once the engine has answered, and the
/// dotted `<ip>:<port>` before that, scheduling the lookup off-task on the
/// first miss. Never blocks the caller, which is the whole point: the CA
/// coordinator raises exceptions from the same task that drives every other
/// channel, and a `getnameinfo` on an address with no PTR record parks that
/// task for the resolver's full timeout.
///
/// Outside a Tokio runtime there is nothing to schedule the lookup on, so the
/// answer stays the dotted IP — the same degradation libca shows before its
/// engine has run.
pub(crate) fn cached_name(addr: SocketAddr) -> String {
    if let Some(name) = cache().get(&addr) {
        return name.clone();
    }
    schedule(addr);
    addr.to_string()
}

/// Start resolving `addr` now, so a later [`cached_name`] has an answer.
///
/// C builds the circuit's `hostNameCache` in the `tcpiiu` constructor
/// (`tcpiiu.cpp:600`), which hands the address to `ipAddrToAsciiEngine`
/// straight away — so by the time anything asks for the name (an exception
/// block, `ca_host_name`), the engine has long since answered. The port
/// resolved lazily on first ASK instead, and the first ask is usually the
/// exception block of a circuit that just died: it got the dotted IP, C got
/// `localhost:5064`. Warming at connect closes that by construction.
pub(crate) fn warm(addr: SocketAddr) {
    if cache().contains_key(&addr) {
        return;
    }
    schedule(addr);
}

/// Off-task reverse lookup, cached on completion. Outside a Tokio runtime
/// there is nothing to schedule on, so the name stays unresolved — the same
/// degradation libca shows before its engine has run.
fn schedule(addr: SocketAddr) {
    if let Ok(rt) = tokio::runtime::Handle::try_current() {
        rt.spawn(async move {
            if let Ok(name) = tokio::task::spawn_blocking(move || ip_addr_to_a(addr)).await {
                cache().insert(addr, name);
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// R18-20: warming at circuit-connect is what makes the *first* ask — the
    /// exception block of a circuit that just died — see the resolved name.
    /// Pre-fix, `cached_name` scheduled the lookup only when first asked, so
    /// that first ask always answered with the dotted IP and the ECA_DISCONN /
    /// ECA_UNRESPTMO blocks printed `Context: "127.0.0.1:5064"` where C prints
    /// `Context: "localhost:5064"`.
    ///
    /// Unix-only: the invariant it exercises — that a warmed loopback address
    /// resolves to a NON-IP name — holds via `/etc/hosts`' loopback PTR, which
    /// unix guarantees. Windows has no guaranteed loopback PTR, so
    /// `getnameinfo(NI_NAMEREQD)` fails and `ip_addr_to_a` correctly falls back
    /// to the dotted IP (its own contract, covered by
    /// [`an_unresolvable_address_falls_back_to_the_dotted_ip`]); the warmed name
    /// would then equal the dotted IP and this assertion would be meaningless.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn warming_at_connect_makes_the_first_ask_see_the_name() {
        // A port no other test warms, so this exercises a genuine cache miss.
        let a: SocketAddr = "127.0.0.1:5199".parse().unwrap();
        warm(a);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while cache().get(&a).is_none() && std::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let got = cached_name(a);
        assert_ne!(
            got, "127.0.0.1:5199",
            "a warmed address answers with the resolved name, as C's \
             `hostNameCache` does by the time anything prints it"
        );
        assert!(got.ends_with(":5199"), "{got}");
    }

    /// `127.0.0.1` reverse-resolves through `/etc/hosts` on every platform the
    /// CA tools run on, and C's `cainfo` prints `Host: localhost:5064` for a
    /// softIoc on the loopback. The port must produce the same shape:
    /// the NAME, then `:`, then the port — never the dotted IP.
    ///
    /// Unix-only: the loopback-PTR invariant holds via `/etc/hosts` on unix,
    /// not on Windows, where CI has no guaranteed 127.0.0.1 PTR record and
    /// `getnameinfo(NI_NAMEREQD)` correctly fails into the dotted-IP fallback.
    #[cfg(unix)]
    #[test]
    fn loopback_resolves_to_a_name_and_keeps_the_port() {
        let a: SocketAddr = "127.0.0.1:5064".parse().unwrap();
        let got = ip_addr_to_a(a);
        let (name, port) = got.rsplit_once(':').expect("`<host>:<port>`");
        assert_eq!(port, "5064");
        // Not asserting `localhost` literally: the PTR for 127.0.0.1 is the
        // host's own `/etc/hosts`, and a machine is free to name it otherwise.
        // What C guarantees — and the port broke — is that it is a NAME.
        assert_ne!(
            name, "127.0.0.1",
            "reverse resolution did not happen: {got}"
        );
        assert!(!name.is_empty());
    }

    /// C's fallback: `ipAddrToHostName` fails (`NI_NAMEREQD` with no PTR) and
    /// `ipAddrToA` falls back to `ipAddrToDottedIP`, port included. `192.0.2.0/24`
    /// is TEST-NET-1 (RFC 5737) — reserved for documentation, so it has no PTR
    /// record anywhere.
    #[test]
    fn an_unresolvable_address_falls_back_to_the_dotted_ip() {
        let a: SocketAddr = "192.0.2.1:5064".parse().unwrap();
        assert_eq!(ip_addr_to_a(a), "192.0.2.1:5064");
    }

    /// The non-blocking half answers immediately on a cold cache — with the
    /// dotted IP, as libca does before its engine replies.
    #[epics_macros_rs::epics_test]
    async fn the_cache_answers_without_blocking_on_a_miss() {
        let a: SocketAddr = "192.0.2.2:5064".parse().unwrap();
        assert_eq!(cached_name(a), "192.0.2.2:5064");
    }
}
