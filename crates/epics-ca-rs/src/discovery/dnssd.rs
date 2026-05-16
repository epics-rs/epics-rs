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

use std::collections::HashSet;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use hickory_resolver::TokioAsyncResolver;
use hickory_resolver::config::{ResolverConfig, ResolverOpts};
use hickory_resolver::proto::rr::{RData, RecordType};

use super::Backend;

/// Cap on service instances processed from a single PTR answer. A
/// hostile or misconfigured zone could return thousands; each instance
/// costs an SRV + A + AAAA round-trip, so an uncapped count turns
/// `discover()` into a DNS-amplification stall during client startup.
const MAX_INSTANCES: usize = 256;

/// Aggregate wall-clock budget for the whole of `discover()` — the PTR
/// query plus every per-instance SRV/A/AAAA lookup. `discover()` runs
/// during `CaClient::new()`; this bounds how long an untrusted zone can
/// delay client construction regardless of instance count or per-query
/// latency.
const RESOLVE_BUDGET: Duration = Duration::from_secs(10);

/// Per-lookup timeout, so one unresponsive PTR/SRV/A/AAAA query cannot
/// consume the entire `RESOLVE_BUDGET` by itself.
const PER_LOOKUP_TIMEOUT: Duration = Duration::from_secs(3);

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

        // The whole of `discover()` — the PTR query plus the per-instance
        // SRV/A/AAAA lookups — is hard-bounded by the aggregate
        // `RESOLVE_BUDGET` deadline: every individual lookup is timed out
        // at `min(PER_LOOKUP_TIMEOUT, remaining budget)`, so total wall
        // time cannot exceed `RESOLVE_BUDGET` plus the latency of at most
        // one in-flight lookup. `discover()` runs during `CaClient::new()`,
        // so this bounds how long an untrusted/slow zone can stall client
        // construction. Whatever resolved before the deadline is returned.
        let deadline = Instant::now() + RESOLVE_BUDGET;
        // Per-lookup timeout clamped to the remaining aggregate budget,
        // so a lookup started late in the phase cannot overshoot it.
        let lookup_timeout =
            || PER_LOOKUP_TIMEOUT.min(deadline.saturating_duration_since(Instant::now()));

        // Step 1: PTR query on the service-type name. RFC 6763 §4.1
        // places PTR records here, one per service instance.
        // `srv_lookup` would NOT work — it issues a single SRV query
        // and does not chase PTR; SRV records do not live at the
        // service-type name.
        let ptr = match tokio::time::timeout(
            lookup_timeout(),
            self.resolver.lookup(svc.as_str(), RecordType::PTR),
        )
        .await
        {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => {
                tracing::warn!(zone = %self.zone, error = %e,
                    "DNS-SD: PTR lookup failed");
                return Vec::new();
            }
            Err(_) => {
                tracing::warn!(zone = %self.zone,
                    "DNS-SD: PTR lookup timed out");
                return Vec::new();
            }
        };

        // Collect the instance FQDNs the PTR records point at. Dedup
        // via a HashSet (O(1)) and cap the count: a hostile zone could
        // return an unbounded PTR answer, and each instance below costs
        // three sequential DNS round-trips.
        let mut seen_instances: HashSet<String> = HashSet::new();
        let mut instances: Vec<String> = Vec::new();
        for rdata in ptr.iter() {
            if let RData::PTR(target) = rdata {
                let name = target.to_string();
                if seen_instances.insert(name.clone()) {
                    instances.push(name);
                    if instances.len() >= MAX_INSTANCES {
                        tracing::warn!(zone = %self.zone, cap = MAX_INSTANCES,
                            "DNS-SD: PTR answer exceeds instance cap; truncating");
                        break;
                    }
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
        // with both ports. Shares the `deadline`/`lookup_timeout` bound
        // established above.
        let mut out_set: HashSet<SocketAddr> = HashSet::new();
        let mut out: Vec<SocketAddr> = Vec::new();
        for instance in &instances {
            if Instant::now() >= deadline {
                tracing::warn!(zone = %self.zone, budget = ?RESOLVE_BUDGET,
                    "DNS-SD: resolve budget exhausted; returning partial result");
                break;
            }
            let srv = match tokio::time::timeout(
                lookup_timeout(),
                self.resolver.srv_lookup(instance.as_str()),
            )
            .await
            {
                Ok(Ok(r)) => r,
                Ok(Err(e)) => {
                    tracing::warn!(zone = %self.zone, instance = %instance, error = %e,
                        "DNS-SD: SRV lookup failed");
                    continue;
                }
                Err(_) => {
                    tracing::warn!(zone = %self.zone, instance = %instance,
                        "DNS-SD: SRV lookup timed out");
                    continue;
                }
            };
            for record in srv.iter() {
                let port = record.port();
                let target = record.target().to_string();
                // A records.
                if let Ok(Ok(v4)) = tokio::time::timeout(
                    lookup_timeout(),
                    self.resolver.ipv4_lookup(target.as_str()),
                )
                .await
                {
                    for ip in v4.iter() {
                        let addr = SocketAddr::new(std::net::IpAddr::V4(**ip), port);
                        if out_set.insert(addr) {
                            out.push(addr);
                        }
                    }
                }
                // AAAA records.
                if let Ok(Ok(v6)) = tokio::time::timeout(
                    lookup_timeout(),
                    self.resolver.ipv6_lookup(target.as_str()),
                )
                .await
                {
                    for ip in v6.iter() {
                        let addr = SocketAddr::new(std::net::IpAddr::V6(**ip), port);
                        if out_set.insert(addr) {
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
