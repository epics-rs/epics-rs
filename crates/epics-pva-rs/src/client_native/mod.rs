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
pub(crate) mod monitor_queue;
pub mod operation;
pub mod ops_v2;
// The UDP SEARCH modules are compiled out on every embedded target: RTEMS
// newlib lacks the `recvmsg`/`IP_PKTINFO` receive path and `local_addr()`
// readback a UDP search needs, and VxWorks is excluded for the same reason
// (`epics-libcom-rs::net`'s `AsyncUdpV4`/`socket2`/`if-addrs` stack builds for
// neither), so an embedded build resolves PVs over TCP name servers alone
// through `search_engine`'s `SearchTransport::NameServersOnly` seam. `search`
// is the legacy standalone search path and `udp` is the client UDP manager.
//
// The gate that states that is `tokio_backend`, not
// `not(epics_embedded_target)` — the predicate `eb873800c` named as wrong
// while it was fixing the timer half of the same two files. Both modules wait
// on a reactor: `search.rs` on `AsyncUdpV4::recv_from` and `udp.rs` on
// `UdpSocket::readable`, each inside a future started through `runtime::task`.
// `exec_backend` — the backend with no reactor — is selected on a *host* build
// too, by `EPICS_RS_BUILD_EXEC_BACKEND=thread`, so the target gate let both
// compile into a build whose workers panic the moment either is driven. The
// seam's own question is which backend runs the code, never which triple built
// it.
#[cfg(tokio_backend)]
pub mod search;
pub mod search_engine;
pub mod server_conn;
#[cfg(tokio_backend)]
pub mod udp;

pub use context::{AssertedIdentity, CacheAction, PvGetResult, PvaClient, PvaClientBuilder};
pub use operation::PvaOperation;

/// The wire decoder, re-exported at its historical path. It moved to
/// [`crate::decode`] when the server stopped importing it through the client
/// (design doc §9 phase 6, item 2); this keeps `client_native::decode::…`
/// resolving for existing callers.
pub use crate::decode;

#[cfg(test)]
mod seam_guard {
    //! Both halves of the runtime-seam rule for the native client, over one
    //! covered set that the module derives rather than lists.
    //!
    //! The two guards below used to carry a file list each, for one question
    //! asked twice. They disagreed: the spawn half swept `udp.rs` and not
    //! `channel.rs`, the timer half swept `channel.rs` and not `udp.rs`, and
    //! between them they named 7 of this module's 11 files. `search.rs`,
    //! `beacon_throttle.rs`, `monitor_queue.rs` and `mod.rs` were in neither.
    //! A hand-written subject list is default-out — a file added to the module
    //! is invisible to every guard over it, and nothing says so.
    //!
    //! [`ANCHORS`] inverts that. The files come from the directory, and a file
    //! with no entry here fails the sweep by name, so classifying a new file
    //! is a step someone takes deliberately instead of one that happens by
    //! omission.
    //!
    //! Needles are assembled with `concat!` so this module's own text cannot
    //! satisfy the checks it performs.

    use source_guard::{Comments, module_dir, production, sweep};

    /// One entry per file of this module: something its production slice must
    /// still contain, so a slice that stopped covering its subject fails here
    /// instead of passing vacuously.
    const ANCHORS: &[(&str, &str)] = &[
        ("beacon_throttle.rs", "impl BeaconTracker"),
        ("channel.rs", "impl ConnectionPool"),
        ("context.rs", "impl PvaClient"),
        ("mod.rs", "pub mod context;"),
        ("monitor_queue.rs", "impl MonitorBacklog"),
        ("operation.rs", "impl<T: Send + 'static> PvaOperation<T>"),
        ("ops_v2.rs", "impl SubscriptionHandle"),
        ("search.rs", "pub async fn search"),
        ("search_engine.rs", "async fn run_engine"),
        ("server_conn.rs", "impl ServerConn"),
        ("udp.rs", "async fn recv_loop"),
    ];

    /// Every file of `client_native`, with its production slice and its
    /// anchor. Panics on a file `ANCHORS` does not classify.
    fn client_files() -> Vec<(&'static str, &'static str, &'static str)> {
        sweep(module_dir!("src/client_native"), &[])
            .into_iter()
            .map(|(label, src)| {
                let anchor = ANCHORS
                    .iter()
                    .find(|(f, _)| *f == label)
                    .unwrap_or_else(|| {
                        panic!(
                            "client_native/{label} is new and no guard classifies it. \
                             Add its production anchor to `ANCHORS`; the two sweeps \
                             below then cover it, which is what a covered set derived \
                             from the module is for."
                        )
                    })
                    .1;
                let prod = production(src, Comments::Strip);
                assert!(
                    prod.contains(anchor),
                    "client_native/{label}: production slice no longer contains \
                     `{anchor}` — the guards over it would pass vacuously"
                );
                (label, prod, anchor)
            })
            .collect()
    }

    /// Every task the native client spawns in production goes through
    /// `epics_base_rs::runtime::task::spawn`, not `tokio::spawn` — the
    /// client-side twin of `server_native::tcp`'s
    /// `connection_scope_spawns_go_through_the_runtime_seam`.
    ///
    /// A bare `tokio::spawn` panics on a thread with no tokio runtime, which is
    /// exactly the thread the blocking client (`ServerConn::connect_blocking`,
    /// stage 2) runs on for the RTEMS target — and it panics at *runtime*, on
    /// the target, not here.
    #[test]
    fn client_scope_spawns_go_through_the_runtime_seam() {
        let literal = concat!("tokio", "::spawn(");
        for (label, prod, _) in client_files() {
            assert_eq!(
                prod.matches(literal).count(),
                0,
                "client_native/{label}: production scope must spawn through \
                 `runtime::task::spawn`, never `{literal}`"
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
    /// The scope used to be "files that compile for `armv7-rtems-eabihf`",
    /// which excused `search.rs` and `udp.rs` because both are
    /// `#[cfg(not(epics_embedded_target))]`. That is the wrong predicate:
    /// `exec_backend` — the runtime-free backend whose workers have no reactor
    /// — is also selected on a *host* build by
    /// `EPICS_RS_BUILD_EXEC_BACKEND=thread` (`epics-libcom-rs/build.rs`), and
    /// a host build compiles both files. The question is which backend the
    /// code can run on, not which target it is built for, so every file is
    /// swept.
    #[test]
    fn client_scope_timers_go_through_the_runtime_seam() {
        let literal = concat!("tokio", "::time::");
        for (label, prod, _) in client_files() {
            assert_eq!(
                prod.matches(literal).count(),
                0,
                "client_native/{label}: production scope must arm timers through \
                 `runtime::task`, never `{literal}` — on a callback band that \
                 panics at runtime, on the target"
            );
        }
    }
}
