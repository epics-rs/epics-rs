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

pub mod access;
pub mod beacon;
pub mod cache;
pub mod command;
pub mod control;
pub mod downstream;
pub mod master;
pub mod putlog;
pub mod pvlist;
pub mod server;
pub mod stats;
pub mod upstream;

pub use access::AccessConfig;
pub use beacon::BeaconAnomaly;
pub use cache::{CacheTimeouts, GwPvEntry, PvCache, PvState};
pub use command::{CommandHandler, GatewayCommand};
pub use downstream::{ConnEventRecv, ConnEventReplay, DownstreamServer, ReplayingReceiver};
pub use master::{RestartPolicy, SuperviseError, supervise};
pub use putlog::{PutLog, PutLogScope, PutOutcome};
pub use pvlist::{EvaluationOrder, PvList, PvListEntry, PvListMatch};
pub use server::{GatewayConfig, GatewayServer};
pub use stats::{Stats, default_stats_prefix};
pub use upstream::UpstreamManager;
