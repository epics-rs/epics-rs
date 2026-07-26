//! Native pvAccess client.
//!
//! Layered structure (mirrors pvxs `src/client*.cpp`):
//!
//! - [`crate::decode`] parses PVA frames coming from the server. It lives at
//!   the crate root, not here: the server frames its own reads with it too,
//!   and it is pure codec with no I/O (see that module's header).
//! - [`server_conn`] manages a persistent TCP virtual circuit
//!   (handshake + framed I/O + reader/writer/heartbeat tasks)
//! - [`search_engine`] handles UDP search broadcast + reply
//!   collection, beacon-driven fast reconnect
//! - [`channel`] per-PV state machine + connection pool
//! - [`ops_v2`] drives GET / PUT / MONITOR / RPC / GET_FIELD
//!   operations on top of an established channel, with automatic
//!   reconnect for monitors
//! - [`context`] the public [`PvaClient`] facade
//!
//! The legacy `crate::client` module is a thin re-export of this one (see
//! `client.rs`), so existing callers like `pvget-rs` keep working.

pub mod beacon_throttle;
pub mod channel;
pub mod context;
pub mod operation;
pub mod ops_v2;
// The UDP SEARCH modules are compiled out on every embedded target: RTEMS
// newlib lacks the `recvmsg`/`IP_PKTINFO` receive path and `local_addr()`
// readback a UDP search needs, and the same module gate excludes VxWorks too
// (`epics-libcom-rs::net`'s `AsyncUdpV4`/`socket2`/`if-addrs` stack is
// host-only for both), so an embedded build resolves PVs over TCP name
// servers alone through `search_engine`'s `SearchTransport::NameServersOnly`
// seam (doc/pvalink-rtems-design.md §4.2). `search` is the legacy standalone
// search path and `udp` is the client UDP manager — both are host-only
// surface (the latter is used by the host-only `pvxvct-rs` tool and the
// search-engine tests).
#[cfg(not(epics_embedded_target))]
pub mod search;
pub mod search_engine;
pub mod server_conn;
#[cfg(not(epics_embedded_target))]
pub mod udp;

pub use context::{AssertedIdentity, CacheAction, PvGetResult, PvaClient, PvaClientBuilder};
pub use operation::PvaOperation;

/// The wire decoder, re-exported at its historical path. It moved to
/// [`crate::decode`] when the server stopped importing it through the client
/// (design doc §9 phase 6, item 2); this keeps `client_native::decode::…`
/// resolving for existing callers.
pub use crate::decode;

#[cfg(test)]
mod spawn_seam_guard {
    /// Every task the native client spawns in production goes through
    /// `epics_base_rs::runtime::task::spawn`, not `tokio::spawn` — the
    /// client-side twin of `server_native::tcp`'s
    /// `connection_scope_spawns_go_through_the_runtime_seam`
    /// (doc/pvalink-rtems-design.md §4.1, stage 3).
    ///
    /// A bare `tokio::spawn` panics on a thread with no tokio runtime, which is
    /// exactly the thread the blocking client (`ServerConn::connect_blocking`,
    /// stage 2) runs on for the RTEMS target — and it panics at *runtime*, on
    /// the target, not here. So this pins it as source inspection: the
    /// production scope of every `client_native` module must contain no
    /// `tokio::spawn` at all.
    ///
    /// The scan covers the whole client rather than one file because, unlike
    /// the server's single per-connection handler, the client spreads its
    /// spawns across `context`/`operation`/`ops_v2`/`server_conn`/`udp`/
    /// `search_engine` (§4.1's table). Each file's production slice ends at its
    /// first column-0 `#[cfg(test)]`, and each is fenced with a positive anchor
    /// so a moved `#[cfg(test)]` cannot shrink the slice into a vacuous pass.
    #[test]
    fn client_scope_spawns_go_through_the_runtime_seam() {
        // (file source, an anchor that must survive in the production slice).
        let files: &[(&str, &str)] = &[
            (include_str!("context.rs"), "impl PvaClient"),
            (
                include_str!("operation.rs"),
                "impl<T: Send + 'static> PvaOperation<T>",
            ),
            (include_str!("ops_v2.rs"), "impl SubscriptionHandle"),
            (include_str!("server_conn.rs"), "impl ServerConn"),
            (include_str!("udp.rs"), "async fn recv_loop"),
            (include_str!("search_engine.rs"), "async fn run_engine"),
        ];
        // Written split so this assertion cannot match its own source text.
        let literal = concat!("tokio", "::spawn(");
        for (src, anchor) in files {
            let prod = match src.find("\n#[cfg(test)]") {
                Some(i) => &src[..i],
                None => src,
            };
            assert!(
                prod.contains(anchor),
                "production slice no longer covers `{anchor}` — the guard would pass vacuously"
            );
            let hits = prod.matches(literal).count();
            assert_eq!(
                hits, 0,
                "client production scope must spawn through \
                 `runtime::task::spawn`; found {hits} bare `{literal}` near `{anchor}`"
            );
        }
    }

    /// Every timer the native client arms in production comes from
    /// `epics_base_rs::runtime::task::{interval, sleep, sleep_until, timeout}`,
    /// not from `tokio::time` — the timer twin of the spawn guard above.
    ///
    /// MEASURED, and this guard exists because the spawn half alone was not
    /// enough: with every spawn already on the seam, the stage-5 target image
    /// still died with three
    /// *"there is no reactor running, must be called from the context of a
    /// Tokio 1.x runtime"* panics on `cbMedium`
    /// (`tokio/src/time/interval.rs:138` from `run_engine`'s tick,
    /// `search_engine.rs`'s NS reconnect sleep, and pvalink's re-subscribe
    /// backoff). A task moved onto the callback pool takes its timer calls with
    /// it, so pinning where tasks *start* says nothing about what they wait on.
    ///
    /// Scope is the files that compile for `armv7-rtems-eabihf` (and the
    /// `*-wrs-vxworks*` triples). `udp.rs` and `search.rs` are
    /// `#[cfg(not(epics_embedded_target))]` (see the module list above) —
    /// they may use `tokio::time` freely, because no embedded-target build
    /// ever contains them.
    #[test]
    fn client_scope_timers_go_through_the_runtime_seam() {
        // (file source, an anchor that must survive in the production slice).
        // The embedded-target-compiled client files only.
        let files: &[(&str, &str)] = &[
            (include_str!("context.rs"), "impl PvaClient"),
            (
                include_str!("operation.rs"),
                "impl<T: Send + 'static> PvaOperation<T>",
            ),
            (include_str!("ops_v2.rs"), "impl SubscriptionHandle"),
            (include_str!("server_conn.rs"), "impl ServerConn"),
            (include_str!("search_engine.rs"), "async fn run_engine"),
            (include_str!("channel.rs"), "impl ConnectionPool"),
        ];
        // Written split so this assertion cannot match its own source text.
        let literal = concat!("tokio", "::time::");
        for (src, anchor) in files {
            let prod = match src.find("\n#[cfg(test)]") {
                Some(i) => &src[..i],
                None => src,
            };
            assert!(
                prod.contains(anchor),
                "production slice no longer covers `{anchor}` — the guard would pass vacuously"
            );
            // The seam's own doc comments may name the type they replace, so
            // only code lines count: a `//`-prefixed line is prose.
            let hits = prod
                .lines()
                .filter(|l| !l.trim_start().starts_with("//"))
                .filter(|l| l.contains(literal))
                .count();
            assert_eq!(
                hits, 0,
                "client production scope must arm timers through \
                 `runtime::task`; found {hits} bare `{literal}` near `{anchor}` — \
                 on RTEMS that panics the callback worker at runtime"
            );
        }
    }
}
