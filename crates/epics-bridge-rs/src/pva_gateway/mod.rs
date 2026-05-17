//! PVA-to-PVA proxy gateway.
//!
//! Mirrors the C++ `pva2pva/p2pApp` gateway at the architectural
//! level: one upstream [`epics_pva_rs::client::PvaClient`] keeps a
//! cache of channels (one per upstream PV name), one downstream
//! [`epics_pva_rs::server_native::PvaServer`] accepts client
//! connections and forwards GET / PUT / MONITOR / GET_FIELD ops
//! through the cache.
//!
//! ## Topology
//!
//! ```text
//!   downstream PVA clients
//!            │
//!            ▼
//!   ┌──────────────────┐         ┌────────────────────────┐
//!   │ PvaServer (DS)   │  uses   │ GatewayChannelSource   │
//!   │ in pva-rs        │────────▶│ (impl ChannelSource)   │
//!   └──────────────────┘         └──────────┬─────────────┘
//!                                           │ lookup / get / put
//!                                           ▼
//!                                ┌────────────────────────┐
//!                                │ ChannelCache           │
//!                                │  PV → UpstreamEntry    │
//!                                │   ├ broadcast::Sender  │  fan-out
//!                                │   └ monitor task       │  (one per PV)
//!                                └──────────┬─────────────┘
//!                                           │ pvmonitor / pvget / pvput
//!                                           ▼
//!                                ┌────────────────────────┐
//!                                │ PvaClient (US)         │
//!                                │ in pva-rs              │
//!                                └──────────┬─────────────┘
//!                                           ▼
//!                                  upstream PVA servers
//! ```
//!
//! ## Lifecycle
//!
//! - **Search** — downstream `has_pv` triggers
//!   [`channel_cache::ChannelCache::lookup`], which opens an upstream
//!   monitor (one per PV) and waits for the first event before
//!   reporting "found". Subsequent searches for the same PV hit the
//!   fast path.
//! - **GET** — uses the cached snapshot; same value the upstream
//!   server would return on a fresh GET.
//! - **MONITOR** — every downstream subscriber receives a fresh
//!   `tokio::sync::broadcast::Receiver`. Slow subscribers see
//!   lagged events; the next upstream tick resyncs.
//! - **PUT** — forwarded through the upstream `PvaClient::pvput`,
//!   reusing the existing upstream channel (no fresh CREATE_CHAN
//!   round-trip per write).
//! - **Cleanup** — a 30 s background tick drops entries that have
//!   neither been touched since the previous tick nor have any live
//!   downstream subscribers. Mirrors p2pApp `cacheClean`.
//! - **Control (B6)** — when a `control_prefix` is set,
//!   [`control::ControlSource`] exposes read-only diagnostic PVs plus
//!   credentialed control RPCs (`<prefix>:flush` / `:drop` /
//!   `:reload`) that flush the channel cache, drop one entry, or
//!   hot-swap the gateway-side ACF policy.
//!
//! ## Quick start
//!
//! ```no_run
//! use std::sync::Arc;
//! use epics_bridge_rs::pva_gateway::{PvaGateway, PvaGatewayConfig};
//!
//! # async fn run() -> epics_bridge_rs::pva_gateway::error::GwResult<()> {
//! let gw = PvaGateway::start(PvaGatewayConfig::default())?;
//! gw.run().await?;
//! # Ok(())
//! # }
//! ```

pub mod channel_cache;
pub mod control;
pub mod error;
pub mod gateway;
pub mod middleware;
pub mod multi_gateway;
pub mod source;

pub use channel_cache::{ChannelCache, DEFAULT_CLEANUP_INTERVAL, UpstreamEntry};
pub use control::{ControlSource, CredentialCheck};
pub use error::{GwError, GwResult};
pub use gateway::{PvaGateway, PvaGatewayConfig};
pub use multi_gateway::{MultiTenantPvaGateway, MultiTenantPvaGatewayBuilder};
pub use source::GatewayChannelSource;
