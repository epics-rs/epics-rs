//! Native pvAccess client — no `spvirit_client` dependency.
//!
//! Layered structure (mirrors pvxs `src/client*.cpp`):
//!
//! - [`decode`] parses PVA frames coming from the server
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
pub mod decode;
pub mod operation;
pub mod ops_v2;
pub mod search;
pub mod search_engine;
pub mod server_conn;

pub use context::{AssertedIdentity, PvGetResult, PvaClient, PvaClientBuilder};
pub use operation::PvaOperation;
