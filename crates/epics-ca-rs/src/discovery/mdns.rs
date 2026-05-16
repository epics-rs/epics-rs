//! mDNS-based discovery — link-local subnet only.
//!
//! Uses the `mdns-sd` crate. Server side announces itself with a
//! self-describing TXT record; client side runs a continuous browser
//! and exposes both an initial snapshot and a subscription stream.

#![cfg(feature = "discovery")]

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use tokio::sync::mpsc;

use super::{Backend, CA_SERVICE_TYPE, DiscoveryEvent};

/// Suffix appended to `CA_SERVICE_TYPE` for mDNS browse/announce.
/// `mdns-sd` requires the trailing `.local.` for link-local domain.
const MDNS_TYPE: &str = "_epics-ca._tcp.local.";

/// Client-side mDNS discovery backend.
///
/// Spawns a `ServiceDaemon` on construction and runs a background
/// browser. Discovered IOCs are pushed into both an internal
/// snapshot (for `discover()`) and a subscriber channel (for
/// `subscribe()`).
pub struct MdnsBackend {
    #[allow(dead_code)]
    daemon: ServiceDaemon,
    snapshot: Arc<Mutex<Vec<SocketAddr>>>,
    event_rx: Mutex<Option<mpsc::UnboundedReceiver<DiscoveryEvent>>>,
}

impl MdnsBackend {
    pub fn new() -> Result<Self, mdns_sd::Error> {
        let daemon = ServiceDaemon::new()?;
        let receiver = daemon.browse(MDNS_TYPE)?;
        let snapshot: Arc<Mutex<Vec<SocketAddr>>> = Arc::new(Mutex::new(Vec::new()));
        let (event_tx, event_rx) = mpsc::unbounded_channel();

        let snap_clone = snapshot.clone();
        tokio::spawn(async move {
            while let Ok(event) = receiver.recv_async().await {
                match event {
                    ServiceEvent::ServiceResolved(info) => {
                        for addr in resolve_addresses(&info) {
                            if let Ok(mut snap) = snap_clone.lock() {
                                if !snap.contains(&addr) {
                                    snap.push(addr);
                                }
                            }
                            let _ = event_tx.send(DiscoveryEvent::Added {
                                instance: info.get_fullname().to_string(),
                                addr,
                            });
                            tracing::info!(addr = %addr, instance = info.get_fullname(),
                                "mDNS discovered IOC");
                        }
                    }
                    ServiceEvent::ServiceRemoved(_, fullname) => {
                        // mdns-sd does not carry the resolved address on
                        // removal, so `addr` is a `0.0.0.0:0` sentinel —
                        // consumers MUST match `Removed` by the `instance`
                        // string (the mDNS fullname), never by `addr`.
                        let _ = event_tx.send(DiscoveryEvent::Removed {
                            instance: fullname.clone(),
                            addr: "0.0.0.0:0".parse().unwrap(),
                        });
                        tracing::info!(instance = %fullname, "mDNS IOC went away");
                    }
                    _ => {}
                }
            }
        });

        Ok(Self {
            daemon,
            snapshot,
            event_rx: Mutex::new(Some(event_rx)),
        })
    }
}

#[async_trait::async_trait]
impl Backend for MdnsBackend {
    async fn discover(&self) -> Vec<SocketAddr> {
        // Give the browser a brief window to populate before the first
        // call returns (otherwise CaClient::new returns instantly with
        // an empty list).
        tokio::time::sleep(Duration::from_millis(500)).await;
        self.snapshot.lock().map(|s| s.clone()).unwrap_or_default()
    }

    fn subscribe(&self) -> Option<mpsc::UnboundedReceiver<DiscoveryEvent>> {
        self.event_rx.lock().ok().and_then(|mut g| g.take())
    }
}

/// Server-side: announce this IOC on the local mDNS responder.
///
/// Hold the returned guard for the IOC's lifetime; dropping it
/// unregisters the service.
pub struct MdnsAnnouncer {
    daemon: ServiceDaemon,
    fullname: String,
}

impl MdnsAnnouncer {
    /// Register `<instance>._epics-ca._tcp.local.` pointing at this
    /// host's local IP and `tcp_port`.
    pub fn announce(
        instance: &str,
        tcp_port: u16,
        txt: Vec<(String, String)>,
    ) -> Result<Self, mdns_sd::Error> {
        let daemon = ServiceDaemon::new()?;
        let hostname = epics_base_rs::runtime::env::hostname();
        let host_target = format!("{hostname}.local.");

        // Discover routable IPv4 addresses on every up, non-loopback
        // interface so multi-homed IOCs announce all paths.
        let ips: Vec<IpAddr> = if_addrs::get_if_addrs()
            .unwrap_or_default()
            .into_iter()
            .filter(|iface| !iface.is_loopback())
            .filter_map(|iface| match iface.ip() {
                IpAddr::V4(v4) => Some(IpAddr::V4(v4)),
                _ => None,
            })
            .collect();

        let info = ServiceInfo::new(
            MDNS_TYPE,
            instance,
            &host_target,
            &ips[..],
            tcp_port,
            &txt[..],
        )?;
        let fullname = info.get_fullname().to_string();
        daemon.register(info)?;
        tracing::info!(instance = %instance, port = tcp_port,
            "mDNS announce registered ({fullname})");
        Ok(Self { daemon, fullname })
    }
}

impl Drop for MdnsAnnouncer {
    fn drop(&mut self) {
        let _ = self.daemon.unregister(&self.fullname);
    }
}

impl MdnsBackend {
    /// Convenience entry point used by `CaServer::run()` so callers
    /// don't need to import `MdnsAnnouncer`.
    pub fn announce_helper(
        instance: &str,
        port: u16,
        txt: Vec<(String, String)>,
    ) -> Result<MdnsAnnouncer, mdns_sd::Error> {
        MdnsAnnouncer::announce(instance, port, txt)
    }
}

fn resolve_addresses(info: &ServiceInfo) -> Vec<SocketAddr> {
    let port = info.get_port();
    info.get_addresses_v4()
        .iter()
        .map(|ip| SocketAddr::new(IpAddr::V4(**ip), port))
        .collect()
}

// Suppress "field is never read" lint — `daemon` keeps the background
// task alive via Drop; the field exists only to extend the lifetime.
#[allow(dead_code)]
fn _suppress_unused() {
    let _ = std::mem::size_of::<MdnsBackend>();
}

#[allow(dead_code)]
const _: fn() = || {
    let _ = CA_SERVICE_TYPE;
};
