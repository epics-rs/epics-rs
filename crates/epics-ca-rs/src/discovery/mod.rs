//! Service discovery for CA — mDNS + DNS-SD.
//!
//! Removes the operational burden of maintaining `EPICS_CA_ADDR_LIST`
//! across every client. IOCs announce themselves; clients discover
//! them automatically.
//!
//! Two transport mechanisms, same `_epics-ca._tcp` service type:
//!
//! - **mDNS** (link-local multicast, RFC 6762) — single subnet only.
//!   Zero infrastructure: pure UDP multicast 224.0.0.251:5353.
//! - **DNS-SD over unicast DNS** (RFC 6763) — works across subnets,
//!   the WAN, anywhere standard DNS reaches. Requires zone records
//!   on a site DNS server.
//!
//! Both are gated behind the `discovery` cargo feature. The bare
//! [`Backend`] trait works without the feature — applications can
//! plug in custom discovery backends (Consul, etcd, site CMDB, ...)
//! without depending on mdns-sd or hickory-resolver.
//!
//! # Trust model
//!
//! Discovery is **unauthenticated**. mDNS multicast and unicast DNS-SD
//! have no notion of identity — a discovered address is whoever
//! answered. A hostile mDNS responder on the LAN, or a poisoned DNS
//! zone, can steer a client onto a rogue IOC, exactly as a spoofed
//! `EPICS_CA_ADDR_LIST` entry could. Discovery decides *where to look*,
//! never *who to trust*: integrity and authenticity of PV traffic must
//! come from the mTLS + capability-token layer, not from discovery.

use std::net::SocketAddr;

mod r#static;

pub use r#static::StaticBackend;

#[cfg(feature = "discovery-dns-update")]
mod dns_update;
#[cfg(feature = "discovery")]
mod dnssd;
#[cfg(feature = "discovery")]
mod mdns;
#[cfg(feature = "discovery")]
mod zone;

#[cfg(feature = "discovery-dns-update")]
pub use dns_update::{DnsRegistration, DnsUpdater, TsigAlgo, TsigKey};
#[cfg(feature = "discovery")]
pub use dnssd::DnsSdBackend;
#[cfg(feature = "discovery")]
pub use mdns::MdnsBackend;
#[cfg(feature = "discovery")]
pub use zone::ZoneSnippet;

/// Standard CA service type. Used by both mDNS announces and DNS-SD
/// PTR records. Format `_<name>._<proto>` per RFC 6763 §4.1.
pub const CA_SERVICE_TYPE: &str = "_epics-ca._tcp";

/// Trait every discovery backend implements. The CA client polls
/// `discover()` once at startup; long-lived backends can also feed
/// updates via [`Backend::subscribe`].
#[async_trait::async_trait]
pub trait Backend: Send + Sync {
    /// Return all IOCs currently known to this backend. Called once
    /// at `CaClient` construction. The result is merged with
    /// `EPICS_CA_ADDR_LIST` and the auto-discovered NIC broadcasts.
    async fn discover(&self) -> Vec<SocketAddr>;

    /// Optional: subscribe to live updates as IOCs come and go. Default
    /// implementation returns `None` meaning the backend is "scan once".
    /// Backends that watch for changes (mDNS, watch-style DNS) override
    /// this to push updates into the search engine.
    fn subscribe(&self) -> Option<tokio::sync::mpsc::UnboundedReceiver<DiscoveryEvent>> {
        None
    }
}

/// Live update from a discovery backend.
///
/// Each event is a precise per-`(instance, addr)` delta: a backend emits
/// one `Added` per address an instance advertises and a matching
/// `Removed` carrying that exact address when it goes away. A multi-homed
/// IOC therefore yields one `Added` per interface, and an instance that
/// re-binds to a new address yields a `Removed` for the old address plus
/// an `Added` for the new one. `Removed.addr` is the real resolved
/// address — consumers may key on it directly (e.g. ref-count shared
/// addresses). The `instance` string (mDNS fullname, or DNS-SD
/// `<instance>._epics-ca._tcp.<zone>`) is carried for diagnostics.
#[derive(Debug, Clone)]
pub enum DiscoveryEvent {
    /// An IOC address just became reachable.
    Added { instance: String, addr: SocketAddr },
    /// An IOC address is no longer reachable. `addr` is the real
    /// resolved address that was previously `Added`.
    Removed { instance: String, addr: SocketAddr },
}

/// What the operator/library author asked for via configuration.
/// Resolved into a concrete set of `Backend` instances by
/// `build_backends`.
#[derive(Debug, Clone)]
pub enum DiscoveryConfig {
    /// Plain `EPICS_CA_ADDR_LIST` style — explicit list of addresses.
    Static(Vec<SocketAddr>),
    /// Discover via mDNS on the link-local segment.
    #[cfg(feature = "discovery")]
    Mdns,
    /// Discover via unicast DNS-SD against a site DNS server.
    /// `zone` is the DNS zone to query, e.g. `facility.local`.
    #[cfg(feature = "discovery")]
    DnsSd { zone: String },
    /// Try multiple backends in order, merging the results.
    Composite(Vec<DiscoveryConfig>),
}

/// Convert a [`DiscoveryConfig`] into runnable backends. Returns
/// `Vec<Box<dyn Backend>>` so call sites stay backend-agnostic.
pub fn build_backends(cfg: DiscoveryConfig) -> Vec<Box<dyn Backend>> {
    match cfg {
        DiscoveryConfig::Static(addrs) => vec![Box::new(StaticBackend::new(addrs))],
        DiscoveryConfig::Composite(items) => items.into_iter().flat_map(build_backends).collect(),
        #[cfg(feature = "discovery")]
        DiscoveryConfig::Mdns => match mdns::MdnsBackend::new() {
            Ok(b) => vec![Box::new(b)],
            Err(e) => {
                tracing::warn!(error = %e, "mDNS backend init failed; skipping");
                vec![]
            }
        },
        #[cfg(feature = "discovery")]
        DiscoveryConfig::DnsSd { zone } => match dnssd::DnsSdBackend::new(zone) {
            Ok(b) => vec![Box::new(b)],
            Err(e) => {
                tracing::warn!(error = %e, "DNS-SD backend init failed; skipping");
                vec![]
            }
        },
    }
}

/// Parse `EPICS_CA_DISCOVERY` env var into a `DiscoveryConfig`.
///
/// Supported syntax (whitespace-separated, evaluated in order):
/// - `mdns`             — enable mDNS
/// - `dnssd:<zone>`     — enable DNS-SD against the given zone
/// - `static:<addr>,..` — static address list (comma-separated)
///
/// Examples:
/// - `EPICS_CA_DISCOVERY=mdns dnssd:facility.local`
/// - `EPICS_CA_DISCOVERY=dnssd:operations.example.org`
///
/// Returns `None` when the env var is unset or empty (no discovery).
pub fn from_env() -> Option<DiscoveryConfig> {
    let raw = epics_base_rs::runtime::env::get("EPICS_CA_DISCOVERY")?;
    let mut items: Vec<DiscoveryConfig> = Vec::new();
    for token in raw.split_whitespace() {
        if let Some(cfg) = parse_token(token) {
            items.push(cfg);
        }
    }
    match items.len() {
        0 => None,
        1 => Some(items.into_iter().next().unwrap()),
        _ => Some(DiscoveryConfig::Composite(items)),
    }
}

fn parse_token(tok: &str) -> Option<DiscoveryConfig> {
    if tok == "mdns" {
        #[cfg(feature = "discovery")]
        {
            return Some(DiscoveryConfig::Mdns);
        }
        #[cfg(not(feature = "discovery"))]
        {
            tracing::warn!("EPICS_CA_DISCOVERY=mdns ignored — built without `discovery` feature");
            return None;
        }
    }
    if let Some(zone) = tok.strip_prefix("dnssd:") {
        #[cfg(feature = "discovery")]
        {
            return Some(DiscoveryConfig::DnsSd {
                zone: zone.to_string(),
            });
        }
        #[cfg(not(feature = "discovery"))]
        {
            let _ = zone;
            tracing::warn!(
                "EPICS_CA_DISCOVERY=dnssd:* ignored — built without `discovery` feature"
            );
            return None;
        }
    }
    if let Some(rest) = tok.strip_prefix("static:") {
        let addrs: Vec<SocketAddr> = rest
            .split(',')
            .filter(|s| !s.is_empty())
            .filter_map(parse_static_addr)
            .collect();
        if addrs.is_empty() {
            tracing::warn!(token = %tok, "EPICS_CA_DISCOVERY static: parsed no addresses");
            return None;
        }
        return Some(DiscoveryConfig::Static(addrs));
    }
    tracing::warn!(token = %tok, "EPICS_CA_DISCOVERY: unrecognized token");
    None
}

/// Parse one comma-separated entry of a `static:` token into a
/// [`SocketAddr`].
///
/// An entry may be `<ip>:<port>` or a bare `<ip>`. A bare address
/// defaults to the resolved CA server port (`EPICS_CA_SERVER_PORT`, or
/// 5064) — consistent with `EPICS_CA_ADDR_LIST` parsing, which passes
/// the same env-resolved port as `default_port` to
/// `server::addr_list::resolve_token` — instead of being silently
/// dropped.
fn parse_static_addr(entry: &str) -> Option<SocketAddr> {
    use std::net::IpAddr;

    if let Ok(addr) = entry.parse::<SocketAddr>() {
        return Some(addr);
    }
    if let Ok(ip) = entry.parse::<IpAddr>() {
        let port = epics_base_rs::runtime::net::ca_server_port();
        return Some(SocketAddr::new(ip, port));
    }
    tracing::warn!(entry = %entry, "EPICS_CA_DISCOVERY static: dropped unparseable address");
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_static_token() {
        let cfg = parse_token("static:10.0.0.1:5064,10.0.0.2:5064").unwrap();
        match cfg {
            DiscoveryConfig::Static(addrs) => assert_eq!(addrs.len(), 2),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn parse_unknown_token_is_none() {
        assert!(parse_token("foo:bar").is_none());
    }

    #[test]
    fn parse_static_token_defaults_bare_addr_to_ca_port() {
        // A port-less entry must default to the standard CA server
        // port (5064), not be silently dropped.
        let cfg = parse_token("static:10.0.0.1,10.0.0.2:5066").unwrap();
        match cfg {
            DiscoveryConfig::Static(addrs) => {
                assert_eq!(addrs.len(), 2);
                assert_eq!(addrs[0].port(), crate::protocol::CA_SERVER_PORT);
                assert_eq!(addrs[1].port(), 5066);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[cfg(feature = "discovery")]
    #[test]
    fn parse_mdns_token() {
        assert!(matches!(parse_token("mdns"), Some(DiscoveryConfig::Mdns)));
    }

    #[cfg(feature = "discovery")]
    #[test]
    fn parse_dnssd_token() {
        let cfg = parse_token("dnssd:facility.local").unwrap();
        match cfg {
            DiscoveryConfig::DnsSd { zone } => assert_eq!(zone, "facility.local"),
            _ => panic!("wrong variant"),
        }
    }
}
