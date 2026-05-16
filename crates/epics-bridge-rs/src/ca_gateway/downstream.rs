//! Downstream CA server adapter for the gateway.
//!
//! Hosts a [`CaServer`] backed by a shadow [`PvDatabase`]. The shadow
//! database is populated by the [`UpstreamManager`] as upstream
//! subscriptions establish. Downstream clients see PVs as if they
//! were normal in-process PVs — the gateway is transparent on the wire.
//!
//! ## Lazy resolution and optional preloading
//!
//! Resolution is lazy on-demand, matching C++ ca-gateway: a downstream
//! search for an unknown name triggers an upstream subscription, and the
//! PV is added to the shadow database only after the upstream IOC
//! responds. This is implemented by `GatewayServer::install_search_resolver`
//! (see `server.rs`), which installs a `PvDatabase::set_search_resolver`
//! hook — the epics-rs equivalent of C++ ca-gateway's
//! `gateServer::pvExistTest()`.
//!
//! Eager pre-subscription is still available as an *opt-in* convenience:
//! when `GatewayConfig::preload_path` is set, `GatewayServer::preload_pvs`
//! subscribes to a known set of upstream PVs at startup. It is not
//! required for lazy resolution to work.

use std::sync::Arc;

use epics_base_rs::server::database::PvDatabase;
use epics_ca_rs::server::{CaServer, ServerConnectionEvent};
use tokio::sync::Mutex;
use tokio::sync::broadcast;

use crate::error::BridgeResult;

/// Downstream CA server adapter.
///
/// Wraps a [`CaServer`] that serves the gateway's shadow [`PvDatabase`].
/// All actual CA protocol handling (search, connect, get, put, monitor)
/// is delegated to `epics-ca-rs`.
///
/// The CaServer is held inside a `Mutex` because [`CaServer::connection_events`]
/// requires `&mut self` to install the broadcast sender on first call.
/// After installation, the server is moved out and run.
pub struct DownstreamServer {
    server: Mutex<Option<CaServer>>,
    /// Cached shadow DB pointer for `database()` accessor.
    shadow_db: Arc<PvDatabase>,
}

impl DownstreamServer {
    /// Create a new downstream server bound to `port`, serving from
    /// the given shadow database.
    pub fn new(shadow_db: Arc<PvDatabase>, port: u16) -> Self {
        let server = CaServer::from_parts(shadow_db.clone(), port, None, None, None, None);
        Self {
            server: Mutex::new(Some(server)),
            shadow_db,
        }
    }

    /// Variant of [`Self::new`] that also wraps every accepted
    /// connection in TLS. The gateway terminates TLS from clients;
    /// upstream traffic remains plaintext (see `upstream.rs` for the
    /// upstream-side TLS hooks). Available with the
    /// `ca-gateway-tls` feature.
    #[cfg(feature = "ca-gateway-tls")]
    pub fn new_with_tls(
        shadow_db: Arc<PvDatabase>,
        port: u16,
        tls: std::sync::Arc<epics_ca_rs::tls::ServerConfig>,
    ) -> Self {
        let mut server = CaServer::from_parts(shadow_db.clone(), port, None, None, None, None);
        server.set_tls(tls);
        Self {
            server: Mutex::new(Some(server)),
            shadow_db,
        }
    }

    /// Get the underlying shadow database.
    pub fn database(&self) -> &Arc<PvDatabase> {
        &self.shadow_db
    }

    /// Subscribe to connection lifecycle events. Must be called BEFORE
    /// [`run`] (which moves the server out of the Mutex).
    pub async fn connection_events(&self) -> Option<broadcast::Receiver<ServerConnectionEvent>> {
        let mut guard = self.server.lock().await;
        guard.as_mut().map(|s| s.connection_events())
    }

    /// Snapshot the beacon-anomaly Notify handle so the gateway can
    /// fire `generateBeaconAnomaly`-style pulses after [`run`] has
    /// consumed the inner CaServer. Must be called BEFORE `run`;
    /// returns None afterwards.
    pub async fn beacon_anomaly_handle(&self) -> Option<Arc<tokio::sync::Notify>> {
        let guard = self.server.lock().await;
        guard.as_ref().map(|s| s.beacon_anomaly_handle())
    }

    /// Run the CA server (blocks until shutdown).
    ///
    /// Spawn this in a tokio task — it accepts incoming TCP connections
    /// from downstream clients, handles UDP search broadcasts, and emits
    /// beacons. After this is called, [`connection_events`] returns None.
    pub async fn run(&self) -> BridgeResult<()> {
        let server = {
            let mut guard = self.server.lock().await;
            match guard.take() {
                Some(s) => s,
                None => {
                    return Err(crate::error::BridgeError::PutRejected(
                        "DownstreamServer already running or consumed".into(),
                    ));
                }
            }
        };
        server
            .run()
            .await
            .map_err(|e| crate::error::BridgeError::PutRejected(format!("CaServer run: {e}")))
    }

    /// Reinstall the inner [`CaServer`] after a previous [`run`] returned.
    /// Used by the supervisor when a CA server task crashes — the outer
    /// supervise loop reconstructs a server (with the same shadow DB)
    /// and re-attaches it here so the next [`run`] picks it up.
    /// Returns the previously installed server, if any.
    pub async fn reinstall(&self, server: CaServer) -> Option<CaServer> {
        let mut guard = self.server.lock().await;
        let prev = guard.take();
        *guard = Some(server);
        prev
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn construct_downstream() {
        let db = Arc::new(PvDatabase::new());
        let downstream = DownstreamServer::new(db.clone(), 0);
        // Just verify it constructs without panicking
        assert!(Arc::ptr_eq(downstream.database(), &db));
    }

    #[tokio::test]
    async fn connection_events_subscribe() {
        let db = Arc::new(PvDatabase::new());
        let downstream = DownstreamServer::new(db, 0);
        let rx = downstream.connection_events().await;
        assert!(rx.is_some(), "expected receiver");
    }
}
