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
pub mod search;
pub mod search_engine;
pub mod server_conn;
pub mod udp;

pub use context::{AssertedIdentity, CacheAction, PvGetResult, PvaClient, PvaClientBuilder};
pub use operation::PvaOperation;

/// The wire decoder, re-exported at its historical path. It moved to
/// [`crate::decode`] when the server stopped importing it through the client
/// (design doc §9 phase 6, item 2); this keeps `client_native::decode::…`
/// resolving for existing callers.
pub use crate::decode;
