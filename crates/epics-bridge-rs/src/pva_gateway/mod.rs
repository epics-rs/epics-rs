//! PVA-to-PVA proxy gateway.
//!
//! Mirrors the C++ `pva2pva/p2pApp` gateway at the architectural
//! level: one upstream [`epics_pva_rs::client::PvaClient`] keeps a
//! cache of channels (one per upstream PV name), one downstream
//! `epics_pva_rs::server_native::PvaServer` accepts client
//! connections and forwards GET / PUT / MONITOR / GET_FIELD ops
//! through the cache. Named in a code span rather than linked because
//! this module doc is compiled on the reactor-free backend, where that
//! type does not exist; the host page still reaches it through
//! `gateway`.
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

pub mod channel_cache;
pub mod control;
pub mod error;
// The two files that stand a downstream `epics_pva_rs::server_native::PvaServer`,
// and only those two. `PvaServer` and its `runtime` are the reactor-bound PVA
// front-end and vanish on the reactor-free backend; the cache, the source, the
// middleware and the control PVs name only `PvaClient` and the `runtime::task`
// seam, which are present on both. Gating the whole module on the front-end's
// predicate is what took all of them out with it.
#[cfg(tokio_backend)]
pub mod gateway;
pub mod middleware;
#[cfg(tokio_backend)]
pub mod multi_gateway;
pub mod source;

pub use channel_cache::{ChannelCache, DEFAULT_CLEANUP_INTERVAL, UpstreamEntry};
pub use control::ControlSource;
pub use error::{GwError, GwResult};
#[cfg(tokio_backend)]
pub use gateway::{PvaGateway, PvaGatewayConfig};
pub use middleware::{
    AclConfig, AuditEvent, AuditEventKind, AuditResult, AuditSink, ClosureAudit, MpscAuditSink,
    NoopAudit,
};
#[cfg(tokio_backend)]
pub use multi_gateway::{MultiTenantPvaGateway, MultiTenantPvaGatewayBuilder};
pub use source::GatewayChannelSource;
