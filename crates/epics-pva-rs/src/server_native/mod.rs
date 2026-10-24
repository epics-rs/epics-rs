//! Native pvAccess server runtime.
//!
//! Two layers, split by whether they touch the network:
//!
//! * **Protocol / source layer** — [`source`], [`shared_pv`], [`composite`],
//!   [`server_info`], [`op_handle`], [`monitor_control`], [`config`],
//!   [`search`], [`search_engine`], [`peers`]. No sockets; drives the codec in
//!   [`crate::decode`] / [`crate::proto`]. Compiles for every target, RTEMS
//!   included.
//! * **Async I/O layer** — `accept`, `udp`, `runtime`. Built on
//!   `tokio::net` (and, underneath it, mio) plus `socket2`.
//!   None of those cross to `armv7-rtems-eabihf` (§8.1), so this layer is
//!   host-only; the embedded target gets a blocking thread-per-client driver
//!   instead (design doc §4, §9 phase 6
//!   item 7), which sits on the protocol layer above. The gate is
//!   `tokio_backend`: the layer needs a reactor, and the embedded target plus
//!   a host `exec_backend` build are exactly the configurations that have
//!   none.
//!
//! `accept` is the TCP side's only socket-bearing module; [`tcp`] and the
//! protocol code behind it speak `AsyncRead`/`AsyncWrite` trait objects, so a
//! second (blocking) driver is an addition beside `accept` rather than a
//! `cfg` threaded through the protocol.
//! The re-point that moves the gate off `tcp` and onto `accept` — the owed
//! half named in `4c75e766` — has landed: `tcp`, [`peers`] and [`search`]
//! build for RTEMS, and the four items that held `tcp` back were fixed at
//! source rather than gated around (config and SEARCH protocol lifted out of
//! the host-only modules that held them, tokio handle annotations routed
//! through the runtime seam, `leaf_convert`'s PUT direction ungated).

#[cfg(tokio_backend)]
pub mod accept;
// Beacon destinations, cycle and frame, shared by the tokio responder and
// the native drivers' beacon thread.
pub(crate) mod beacon;
// The blocking thread-per-connection driver — the second driver beside
// `accept`, for targets with no reactor (RTEMS item 5 stage 3). Owns
// sockets, so it belongs to the I/O layer; host-compiled and host-tested so
// hosted behaviour can be shown unchanged.
pub mod blocking;
pub mod composite;
// Which of the two native drivers an image runs. The two have one runtime
// surface, so the fork belongs at one call site rather than at every consumer
// of a server handle.
pub mod driver;
// The server config record. No socket, no async — see the module doc for why
// it is not part of [`runtime`].
pub mod config;
pub mod monitor_control;
pub mod op_handle;
// Per-connection accounting for the report (`pvxsr`). Keyed by peer address
// and driven from [`tcp`], but it holds no socket of its own.
pub mod peers;
// What `pvxsr` reads, from the tokio server and the native drivers alike.
pub mod report;
#[cfg(tokio_backend)]
pub mod runtime;
// SEARCH parse / name-match / response framing. Protocol only — both the UDP
// responders and the TCP-circuit handler feed it bytes they read themselves.
// The readiness-poller driver — the third beside `accept` and `blocking`, for
// targets with no tokio reactor that can still afford one of their own. Owns
// sockets, so it belongs to the I/O layer; host-compiled and host-tested for
// the same reason `blocking` is. Gated as the readiness layer it sits on is:
// `unix` only, so Windows keeps `accept` and `blocking`.
#[cfg(unix)]
pub mod reactor;
pub mod search;
// One UDP datagram's worth of SEARCH decode on top of [`search`]: chained
// message drain, ORIGIN_TAG forward decision, reply-destination resolution,
// source filter. Returns the datagrams to send instead of sending them, so it
// holds no socket either — [`udp`] is the async caller, the blocking RTEMS
// responder is the other.
pub mod search_engine;
// The native drivers bound and served on their own threads: the one exec
// serving path, for `realtime-pva-ioc` and the IOC runners alike.
pub mod serving;
// The TCP listener set every driver binds, from the expanded interface list.
pub(crate) mod serv_iface;
pub mod server_info;
pub mod shared_pv;
pub mod source;
// The circuit protocol. Speaks `AsyncRead`/`AsyncWrite` trait objects and
// spawns through the runtime seam — no socket type, so it is target-neutral;
// `accept` supplies the hosted stream and the embedded-target blocking
// driver will supply its own.
pub mod tcp;
#[cfg(tokio_backend)]
pub mod udp;

pub use composite::CompositeSource;
pub use monitor_control::{MonitorControlOp, MonitorReceiver, PostError};
pub use op_handle::{
    ClientCredentials, ExecOp, ExecResult, MessageLevel, OpBase, OpMessage, RemoteLogger,
};
// The config record lives in the ungated [`config`] module and compiles for
// every target, so it is re-exported from there rather than from the host-only
// [`runtime`] that also re-exports it. Routing it through `runtime` made
// `server_native::PvaServerConfig` vanish on `armv7-rtems-eabihf` even though
// the type was right there, which forced embedded callers onto
// `server_native::config::PvaServerConfig` and left every doc reference to the
// short path unresolvable on that target.
pub use config::{DEFAULT_MAX_MESSAGE_SIZE, PvaServerConfig};
pub use peers::{ChannelReport, MonitorReport, PeerEntry, PeerRegistry, PeerSnapshot};
pub use report::{Liveness, LivenessGuard, ServerReport, ServerReportHandle};
#[cfg(tokio_backend)]
pub use runtime::{BoundPvaServer, PvaServer, run_pva_server};
pub use server_info::{
    BUILTIN_SOURCE_NAME, SERVER_PV_NAME, SERVER_SOURCE_NAME, ServerInfoSource, USER_SOURCE_NAME,
};
pub use shared_pv::{AddPvError, MonitorOutbox, MonitorRing, SharedPV, SharedSource, SquashTail};
pub use source::{
    ChannelContext, ChannelInvalidator, ChannelSource, ChannelSourceObj, DynSource, Marks,
    MonitorOptions, MonitorStream, MonitorUpdate, OpError, OpErrorKind, PostValue, RawMonitorEvent,
    RegistryEntry, SourceShow, UpstreamMonitor, plain_monitor_updates,
};
