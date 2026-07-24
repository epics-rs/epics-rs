//! Native pvAccess server runtime.
//!
//! Two layers, split by whether they touch the network:
//!
//! * **Protocol / source layer** — [`source`], [`shared_pv`], [`composite`],
//!   [`server_info`], [`op_handle`], [`monitor_control`], [`config`],
//!   [`search`], [`search_engine`], [`peers`]. No sockets; drives the codec in
//!   [`crate::decode`] / [`crate::proto`]. Compiles for every target, RTEMS
//!   included.
//! * **Async I/O layer** — [`accept`], [`udp`], [`runtime`]. Built on
//!   `tokio::net` (and, underneath it, mio) plus `socket2` / `if-addrs`.
//!   None of those cross to `armv7-rtems-eabihf` or the `*-wrs-vxworks*`
//!   triples (§8.1), so this layer is host-only; each embedded target gets a
//!   blocking thread-per-client driver instead (design doc §4, §9 phase 6
//!   item 7), which sits on the protocol layer above. The gate is
//!   `epics_embedded_target`, not a feature: it is not a choice a hosted
//!   build can make.
//!
//! [`accept`] is the TCP side's only socket-bearing module; [`tcp`] and the
//! protocol code behind it speak `AsyncRead`/`AsyncWrite` trait objects, so a
//! second (blocking) driver is an addition beside [`accept`] rather than a
//! `cfg` threaded through the protocol (`doc/pva-rtems-item7-design.md` §6).
//! The re-point that moves the gate off `tcp` and onto [`accept`] — the owed
//! half named in `4c75e766` — has landed: `tcp`, [`peers`] and [`search`]
//! build for RTEMS, and the four items that held `tcp` back were fixed at
//! source rather than gated around (config and SEARCH protocol lifted out of
//! the host-only modules that held them, tokio handle annotations routed
//! through the runtime seam, `leaf_convert`'s PUT direction ungated).

#[cfg(not(epics_embedded_target))]
pub mod accept;
// The blocking thread-per-connection driver — the second driver beside
// [`accept`], for targets with no reactor (RTEMS item 5 stage 3). Owns
// sockets, so it belongs to the I/O layer; host-compiled and host-tested so
// hosted behaviour can be shown unchanged.
pub mod blocking;
pub mod composite;
// The server config record. No socket, no async — see the module doc for why
// it is not part of [`runtime`].
pub mod config;
pub mod monitor_control;
pub mod op_handle;
// Per-connection accounting for the report (`pvxsr`). Keyed by peer address
// and driven from [`tcp`], but it holds no socket of its own.
pub mod peers;
#[cfg(not(epics_embedded_target))]
pub mod runtime;
// SEARCH parse / name-match / response framing. Protocol only — both the UDP
// responders and the TCP-circuit handler feed it bytes they read themselves.
pub mod search;
// One UDP datagram's worth of SEARCH decode on top of [`search`]: chained
// message drain, ORIGIN_TAG forward decision, reply-destination resolution,
// source filter. Returns the datagrams to send instead of sending them, so it
// holds no socket either — [`udp`] is the async caller, the blocking RTEMS
// responder is the other.
pub mod search_engine;
pub mod server_info;
pub mod shared_pv;
pub mod source;
// The circuit protocol. Speaks `AsyncRead`/`AsyncWrite` trait objects and
// spawns through the runtime seam — no socket type, so it is target-neutral;
// [`accept`] supplies the hosted stream and the embedded-target blocking
// driver will supply its own.
pub mod tcp;
#[cfg(not(epics_embedded_target))]
pub mod udp;

pub use composite::CompositeSource;
pub use monitor_control::{MonitorControlOp, MonitorReceiver, PostError};
pub use op_handle::{
    ClientCredentials, ExecOp, ExecResult, MessageLevel, OpBase, OpMessage, RemoteLogger,
};
pub use peers::{ChannelReport, PeerEntry, PeerRegistry, PeerSnapshot};
#[cfg(not(epics_embedded_target))]
pub use runtime::{
    DEFAULT_MAX_MESSAGE_SIZE, PvaServer, PvaServerConfig, ServerReportHandle, run_pva_server,
};
pub use server_info::{SERVER_PV_NAME, SERVER_SOURCE_NAME, ServerInfoSource};
pub use shared_pv::{AddPvError, SharedPV, SharedSource};
pub use source::{
    ChannelContext, ChannelInvalidator, ChannelSource, ChannelSourceObj, DynSource, MonitorOptions,
    MonitorStream, MonitorUpdate, OpError, OpErrorKind, RawMonitorEvent, UpstreamMonitor,
    plain_monitor_updates,
};
