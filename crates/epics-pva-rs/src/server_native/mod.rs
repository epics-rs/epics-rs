//! Native pvAccess server runtime.
//!
//! Two layers, split by whether they touch the network:
//!
//! * **Protocol / source layer** — [`source`], [`shared_pv`], [`composite`],
//!   [`server_info`], [`op_handle`], [`monitor_control`], [`config`],
//!   [`search`], [`peers`]. No sockets; drives the codec in
//!   [`crate::decode`] / [`crate::proto`]. Compiles for every target, RTEMS
//!   included.
//! * **Async I/O layer** — [`accept`], [`tcp`], [`udp`], [`runtime`]. Built
//!   on `tokio::net` (and, underneath it, mio) plus `socket2` / `if-addrs`.
//!   None of those cross to `armv7-rtems-eabihf` (§8.1), so this layer is
//!   host-only; RTEMS gets a blocking thread-per-client driver instead
//!   (design doc §4, §9 phase 6 item 7), which will sit on the protocol
//!   layer above. The gate is the target, not a feature: it is not a
//!   choice a hosted build can make.
//!
//! [`accept`] is the TCP side's only socket-bearing module; [`tcp`] and the
//! protocol code behind it speak `AsyncRead`/`AsyncWrite` trait objects, so a
//! second (blocking) driver is an addition beside [`accept`] rather than a
//! `cfg` threaded through the protocol (`doc/pva-rtems-item7-design.md` §6).
//! (`tcp` itself is still under the host-only gate pending the re-point that
//! moves the gate to [`accept`] — the owed half named in `4c75e766`.)

#[cfg(not(target_os = "rtems"))]
pub mod accept;
pub mod composite;
// The server config record. No socket, no async — see the module doc for why
// it is not part of [`runtime`].
pub mod config;
pub mod monitor_control;
pub mod op_handle;
// Per-connection accounting for the report (`pvxsr`). Every reader and
// writer is a TCP connection, so it belongs to the I/O layer below.
#[cfg(not(target_os = "rtems"))]
pub mod peers;
#[cfg(not(target_os = "rtems"))]
pub mod runtime;
// SEARCH parse / name-match / response framing. Protocol only — both the UDP
// responders and the TCP-circuit handler feed it bytes they read themselves.
pub mod search;
pub mod server_info;
pub mod shared_pv;
pub mod source;
#[cfg(not(target_os = "rtems"))]
pub mod tcp;
#[cfg(not(target_os = "rtems"))]
pub mod udp;

pub use composite::CompositeSource;
pub use monitor_control::{MonitorControlOp, MonitorReceiver, PostError};
pub use op_handle::{
    ClientCredentials, ExecOp, ExecResult, MessageLevel, OpBase, OpMessage, RemoteLogger,
};
#[cfg(not(target_os = "rtems"))]
pub use peers::{ChannelReport, PeerEntry, PeerRegistry, PeerSnapshot};
#[cfg(not(target_os = "rtems"))]
pub use runtime::{PvaServer, PvaServerConfig, ServerReportHandle, run_pva_server};
pub use server_info::{SERVER_PV_NAME, SERVER_SOURCE_NAME, ServerInfoSource};
pub use shared_pv::{AddPvError, SharedPV, SharedSource};
pub use source::{
    ChannelContext, ChannelInvalidator, ChannelSource, ChannelSourceObj, DynSource, MonitorOptions,
    MonitorStream, MonitorUpdate, OpError, OpErrorKind, RawMonitorEvent, UpstreamMonitor,
    plain_monitor_updates,
};
