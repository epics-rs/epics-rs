//! Unicast DNS-SD discovery — works across subnets / WAN.
//!
//! Implements the RFC 6763 lookup chain explicitly:
//!
//! 1. **PTR** query on the service-type name `_epics-ca._tcp.<zone>` —
//!    enumerates the service-instance names.
//! 2. **SRV** query on each `<instance>._epics-ca._tcp.<zone>` — yields
//!    the target host and TCP port for that instance.
//! 3. **A/AAAA** query on each SRV record's own `target()` hostname —
//!    yields the IP addresses for *that* instance only.
//!
//! This layout matches the records emitted by `zone.rs`
//! (`ZoneSnippet`) and `dns_update.rs` (`DnsUpdater`) in this crate:
//! PTR lives at `_epics-ca._tcp.<zone>`, SRV/TXT at
//! `<instance>._epics-ca._tcp.<zone>`.
//!
//! Uses `hickory-resolver` configured from the system's DNS settings
//! (`/etc/resolv.conf` on Unix, registry on Windows).

#![cfg(feature = "discovery")]

use std::net::SocketAddr;

use hickory_resolver::TokioAsyncResolver;
use hickory_resolver::config::{ResolverConfig, ResolverOpts};
use hickory_resolver::proto::rr::{RData, RecordType};

use super::Backend;

pub struct DnsSdBackend {
    zone: String,
    resolver: TokioAsyncResolver,
}

impl DnsSdBackend {
    pub fn new(zone: impl Into<String>) -> Result<Self, std::io::Error> {
        // Try the system resolver first; fall back to a default config
        // (Cloudflare DNS) if that fails.
        let resolver = match hickory_resolver::system_conf::read_system_conf() {
            Ok((cfg, opts)) => TokioAsyncResolver::tokio(cfg, opts),
            Err(e) => {
                tracing::warn!(error = %e, "system DNS config unavailable; using defaults");
                TokioAsyncResolver::tokio(ResolverConfig::default(), ResolverOpts::default())
            }
        };
        Ok(Self {
            zone: zone.into(),
            resolver,
        })
    }

    /// Service-type FQDN for this backend's zone, e.g.
    /// `_epics-ca._tcp.facility.local`.
    fn service_fqdn(&self) -> String {
        format!("_epics-ca._tcp.{}", self.zone)
    }
}

#[async_trait::async_trait]
impl Backend for DnsSdBackend {
    async fn discover(&self) -> Vec<SocketAddr> {
        let svc = self.service_fqdn();

        // Step 1: PTR query on the service-type name. RFC 6763 §4.1
        // places PTR records here, one per service instance.
        // `srv_lookup` would NOT work — it issues a single SRV query
        // and does not chase PTR; SRV records do not live at the
        // service-type name.
        let ptr = match self.resolver.lookup(svc.as_str(), RecordType::PTR).await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(zone = %self.zone, error = %e,
                    "DNS-SD: PTR lookup failed");
                return Vec::new();
            }
        };

        // Collect the instance FQDNs the PTR records point at.
        let mut instances: Vec<String> = Vec::new();
        for rdata in ptr.iter() {
            if let RData::PTR(target) = rdata {
                let name = target.to_string();
                if !instances.contains(&name) {
                    instances.push(name);
                }
            }
        }
        if instances.is_empty() {
            tracing::debug!(zone = %self.zone, "DNS-SD: no PTR instances found");
            return Vec::new();
        }

        // Step 2+3: for each instance, SRV-resolve to (target host,
        // port), then A/AAAA-resolve that SRV's *own* target. Pairing
        // each IP with that SRV's port avoids the cartesian-product
        // bug where two IOCs on ports 5064/5066 would each be emitted
        // with both ports.
        let mut out: Vec<SocketAddr> = Vec::new();
        for instance in &instances {
            let srv = match self.resolver.srv_lookup(instance.as_str()).await {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(zone = %self.zone, instance = %instance, error = %e,
                        "DNS-SD: SRV lookup failed");
                    continue;
                }
            };
            for record in srv.iter() {
                let port = record.port();
                let target = record.target().to_string();
                // A records.
                if let Ok(v4) = self.resolver.ipv4_lookup(target.as_str()).await {
                    for ip in v4.iter() {
                        let addr = SocketAddr::new(std::net::IpAddr::V4(**ip), port);
                        if !out.contains(&addr) {
                            out.push(addr);
                        }
                    }
                }
                // AAAA records.
                if let Ok(v6) = self.resolver.ipv6_lookup(target.as_str()).await {
                    for ip in v6.iter() {
                        let addr = SocketAddr::new(std::net::IpAddr::V6(**ip), port);
                        if !out.contains(&addr) {
                            out.push(addr);
                        }
                    }
                }
            }
        }

        if out.is_empty() {
            tracing::debug!(zone = %self.zone,
                instances = instances.len(),
                "DNS-SD: instances found but no addresses resolved");
        } else {
            tracing::info!(zone = %self.zone, count = out.len(),
                "DNS-SD discovered IOCs");
        }
        out
    }
}
