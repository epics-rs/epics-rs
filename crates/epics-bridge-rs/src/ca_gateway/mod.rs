//! # ca_gateway — CA fan-out gateway (C++ ca-gateway equivalent)
//!
//! Pure Rust port of [EPICS ca-gateway](https://github.com/epics-modules/ca-gateway).
//! A Channel Access proxy that:
//!
//! - Accepts downstream client connections (CA server side)
//! - Connects to upstream IOCs (CA client side)
//! - Caches PV values and fans out monitor events to multiple clients
//! - Applies access security rules from a `.pvlist` file
//! - Supports PV name aliasing with regex backreferences
//! - Tracks per-PV statistics and exposes them as PVs
//!
//! ## Architecture
//!
//! ```text
//! Upstream IOCs                Gateway                 Downstream Clients
//! ┌─────────┐                ┌─────────┐               ┌─────────┐
//! │ IOC #1  │ ◄── CaClient ──┤         ├── CaServer ──►│ caget   │
//! └─────────┘                │ PvCache │               └─────────┘
//! ┌─────────┐                │  + ACL  │               ┌─────────┐
//! │ IOC #2  │ ◄── CaClient ──┤  + Stats├── CaServer ──►│  CSS    │
//! └─────────┘                │         │               └─────────┘
//!                            └─────────┘                  (~1000)
//! ```
//!
//! ## Sub-modules
//!
//! - [`cache`] — PvCache, GwPvEntry, PvState (5-state FSM)
//! - [`pvlist`] — `.pvlist` configuration file parser
//! - [`access`] — access security adapter (epics-base-rs ACF)
//! - [`upstream`] — CaClient adapter
//! - [`downstream`] — CaServer adapter
//! - [`stats`] — gateway statistics PVs
//! - [`server`] — GatewayServer top-level
// The gateway's serving half — the CA client circuits and the CA server
// front-end — is `tokio_backend`-only, so on `exec_backend` the helpers and
// imports that exist for it are unreferenced while the pure-configuration half
// below still compiles and is still tested. The default build lints the module
// in full.
//
// The same asymmetry is why `UpstreamManager`, `DownstreamServer`,
// `CommandHandler`, `GatewayServer` and `spawn_control_owner` are named in
// code spans rather than linked from the docs of their neighbours: those
// neighbours are compiled in every configuration and these five are not, so an
// intra-doc link to them resolves in none of the reactor-free ones. Gating the
// modules instead would have closed the links by deleting their unit tests
// from the exec-backend suite.
#![cfg_attr(exec_backend, allow(dead_code, unused_imports))]

pub mod access;
pub mod beacon;
pub mod cache;
pub mod command;
pub mod control;
pub mod downstream;
pub mod master;
pub mod putlog;
pub mod pvlist;
pub mod report;
pub mod routing;
pub mod server;
pub mod stats;
pub mod upstream;

pub use access::AccessConfig;
pub use beacon::BeaconAnomaly;
pub use cache::{CacheTimeouts, GwPvEntry, PvCache, PvState};
#[cfg(tokio_backend)]
pub use command::{CommandHandler, GatewayCommand};
#[cfg(tokio_backend)]
pub use downstream::{ConnEventRecv, ConnEventReplay, DownstreamServer, ReplayingReceiver};
pub use master::{RestartPolicy, SuperviseError, supervise};
pub use putlog::{PutLog, PutLogLine, PutLogScope, PutOutcome};
pub use pvlist::{EvaluationOrder, PvList, PvListEntry, PvListMatch};
pub use routing::routing_env_pairs;
#[cfg(tokio_backend)]
pub use server::{CacheMode, GatewayConfig, GatewayServer, resolve_event_mask};
pub use stats::{Stats, default_stats_prefix};
#[cfg(tokio_backend)]
pub use upstream::UpstreamManager;
