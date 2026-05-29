//! Native pvAccess server runtime — no `spvirit_server` dependency.

pub mod composite;
pub mod monitor_control;
pub mod op_handle;
pub mod peers;
pub mod runtime;
pub mod server_info;
pub mod shared_pv;
pub mod source;
pub mod tcp;
pub mod udp;

pub use composite::CompositeSource;
pub use monitor_control::{MonitorControlOp, MonitorReceiver, PostError};
pub use op_handle::{
    ClientCredentials, ExecOp, ExecResult, MessageLevel, OpBase, OpMessage, RemoteLogger,
};
pub use peers::{ChannelReport, PeerEntry, PeerRegistry, PeerSnapshot};
pub use runtime::{PvaServer, PvaServerConfig, run_pva_server};
pub use server_info::{SERVER_PV_NAME, SERVER_SOURCE_NAME, ServerInfoSource};
pub use shared_pv::{SharedPV, SharedSource};
pub use source::{
    ChannelContext, ChannelSource, ChannelSourceObj, DynSource, MonitorOptions, MonitorUpdate,
    OpError, OpErrorKind, RawMonitorEvent, plain_monitor_updates,
};
