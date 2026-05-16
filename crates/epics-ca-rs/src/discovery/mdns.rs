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

/// Cap on tracked mDNS service instances. mDNS is unauthenticated, so
/// without a cap a hostile LAN responder could flood unique instance
/// names and grow the browser's `known` map — and the downstream
/// `addr_refs` / search-engine `addr_list` — without bound. Mirrors
/// the DNS-SD `MAX_INSTANCES`.
const MAX_MDNS_INSTANCES: usize = 256;

/// Client-side mDNS discovery backend.
///
/// Spawns an `mdns-sd` `ServiceDaemon` (which runs its own OS thread)
/// and a background browser task. Discovered IOCs are pushed into both
/// an internal snapshot (for `discover()`) and a subscriber channel
/// (for `subscribe()`). Dropping the backend runs [`Drop`], which
/// calls `ServiceDaemon::shutdown()` and aborts the browser task —
/// dropping the `ServiceDaemon` handle alone does NOT stop its thread.
pub struct MdnsBackend {
    daemon: ServiceDaemon,
    browser: tokio::task::JoinHandle<()>,
    snapshot: Arc<Mutex<Vec<SocketAddr>>>,
    event_rx: Mutex<Option<mpsc::UnboundedReceiver<DiscoveryEvent>>>,
}

impl Drop for MdnsBackend {
    fn drop(&mut self) {
        // The mdns-sd daemon runs on its own OS thread that exits ONLY
        // on an explicit Exit command — dropping the `ServiceDaemon`
        // (just a Sender clone) leaves the thread, its sockets, and the
        // browser task running forever. `shutdown()` sends Exit, which
        // also closes the `ServiceEvent` channel so the browser task
        // falls out of its recv loop; `abort()` is belt-and-suspenders.
        let _ = self.daemon.shutdown();
        self.browser.abort();
    }
}

impl MdnsBackend {
    pub fn new() -> Result<Self, mdns_sd::Error> {
        let daemon = ServiceDaemon::new()?;
        let receiver = daemon.browse(MDNS_TYPE)?;
        let snapshot: Arc<Mutex<Vec<SocketAddr>>> = Arc::new(Mutex::new(Vec::new()));
        let (event_tx, event_rx) = mpsc::unbounded_channel();

        let snap_clone = snapshot.clone();
        let browser = tokio::spawn(async move {
            // Per-instance set of addresses last advertised under that
            // mDNS fullname. Each `ServiceResolved` carries the instance's
            // *complete* current address set, so we diff against this to
            // emit precise per-address `Added`/`Removed` deltas: a
            // multi-homed IOC keeps every interface, and an instance that
            // re-binds to a new address has the old one retracted. This is
            // also how `Removed` gets a real address — the mdns-sd goodbye
            // packet itself carries none.
            let mut known: std::collections::HashMap<
                String,
                std::collections::HashSet<SocketAddr>,
            > = std::collections::HashMap::new();
            while let Ok(event) = receiver.recv_async().await {
                match event {
                    ServiceEvent::ServiceResolved(info) => {
                        let fullname = info.get_fullname().to_string();
                        // Cap distinct instances against an mDNS flood.
                        // An update to an already-tracked instance is
                        // always allowed; only brand-new names past the
                        // cap are dropped.
                        if !known.contains_key(&fullname)
                            && known.len() >= MAX_MDNS_INSTANCES
                        {
                            tracing::warn!(instance = %fullname,
                                cap = MAX_MDNS_INSTANCES,
                                "mDNS: instance cap reached; ignoring new instance");
                            continue;
                        }
                        let resolved: std::collections::HashSet<SocketAddr> =
                            resolve_addresses(&info).into_iter().collect();
                        let prev = known.entry(fullname.clone()).or_default();
                        // Newly-advertised addresses.
                        for &addr in resolved.difference(prev) {
                            if let Ok(mut snap) = snap_clone.lock() {
                                if !snap.contains(&addr) {
                                    snap.push(addr);
                                }
                            }
                            let _ = event_tx.send(DiscoveryEvent::Added {
                                instance: fullname.clone(),
                                addr,
                            });
                            tracing::info!(addr = %addr, instance = %fullname,
                                "mDNS discovered IOC");
                        }
                        // Addresses this instance no longer advertises
                        // (interface removed, or a host/port re-bind).
                        for &addr in prev.difference(&resolved) {
                            if let Ok(mut snap) = snap_clone.lock() {
                                snap.retain(|a| *a != addr);
                            }
                            let _ = event_tx.send(DiscoveryEvent::Removed {
                                instance: fullname.clone(),
                                addr,
                            });
                            tracing::info!(addr = %addr, instance = %fullname,
                                "mDNS IOC address withdrawn");
                        }
                        *prev = resolved;
                    }
                    ServiceEvent::ServiceRemoved(_, fullname) => {
                        // The goodbye packet carries no address; emit a
                        // `Removed` for every address the instance held.
                        if let Some(addrs) = known.remove(&fullname) {
                            for addr in addrs {
                                if let Ok(mut snap) = snap_clone.lock() {
                                    snap.retain(|a| *a != addr);
                                }
                                let _ = event_tx.send(DiscoveryEvent::Removed {
                                    instance: fullname.clone(),
                                    addr,
                                });
                            }
                        }
                        tracing::info!(instance = %fullname, "mDNS IOC went away");
                    }
                    _ => {}
                }
            }
        });

        Ok(Self {
            daemon,
            browser,
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
        // Send the goodbye packet, then stop the daemon. Without
        // `shutdown()` the mdns-sd OS thread (and its sockets) outlives
        // the announcer forever — same leak as `MdnsBackend`. Commands
        // are processed in order, so the unregister goodbye is queued
        // ahead of the Exit.
        let _ = self.daemon.unregister(&self.fullname);
        let _ = self.daemon.shutdown();
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

#[allow(dead_code)]
const _: fn() = || {
    let _ = CA_SERVICE_TYPE;
};
