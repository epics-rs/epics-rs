//! Dynamic DNS UPDATE (RFC 2136) for self-registering IOCs.
//!
//! On startup an IOC sends a single UPDATE message to the configured
//! DNS server adding three records:
//!
//! - `_epics-ca._tcp.<zone>           PTR  <instance>._epics-ca._tcp.<zone>`
//! - `<instance>._epics-ca._tcp.<zone> SRV  0 0 <port> <host>.<zone>`
//! - `<instance>._epics-ca._tcp.<zone> TXT  "k=v" "k=v" ...`
//!
//! A background task refreshes those records every `keepalive`
//! period so they don't age out of the zone, and the `Drop` impl
//! sends DELETE updates so a graceful shutdown removes the records
//! immediately.
//!
//! Authentication is via TSIG (RFC 2845) — a shared symmetric key
//! the DNS admin issues from `tsig-keygen -a hmac-sha256 epics-key`.
//! Without a key the UPDATE is sent unsigned and most production
//! servers will reject it (BIND default `update-policy` requires
//! TSIG). For development with `update-policy { grant local-ddns
//! zonesub ANY; };` the unsigned path works.

#![cfg(feature = "discovery-dns-update")]

use std::net::SocketAddr;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use hickory_client::client::{AsyncClient, ClientHandle, Signer};
use hickory_client::error::ClientError;
use hickory_client::rr::rdata::{SRV, TXT};
use hickory_client::rr::{Name, RData, Record};
use hickory_client::tcp::TcpClientStream;
use hickory_proto::iocompat::AsyncIoTokioAsStd;
use hickory_proto::rr::dnssec::rdata::tsig::TsigAlgorithm;
use hickory_proto::rr::dnssec::tsig::TSigner;

use super::tsig::{TsigAlgo, TsigKey};
use tokio::net::TcpStream as TokioTcpStream;

impl TsigAlgo {
    fn to_proto(self) -> TsigAlgorithm {
        match self {
            TsigAlgo::HmacSha256 => TsigAlgorithm::HmacSha256,
            TsigAlgo::HmacSha512 => TsigAlgorithm::HmacSha512,
        }
    }
}

/// What an IOC wants to register.
#[derive(Debug, Clone)]
pub struct DnsRegistration {
    /// DNS server to send UPDATE messages to (e.g. `10.0.0.1:53`).
    pub server: SocketAddr,
    /// Zone the IOC belongs to (e.g. `facility.local.` — trailing dot recommended).
    pub zone: String,
    /// Service-instance label (e.g. `motor-ioc`). Becomes
    /// `<instance>._epics-ca._tcp.<zone>`.
    pub instance: String,
    /// Hostname target for the SRV record. Must already have an A record
    /// in the same zone (or another resolvable zone). Example: `motor-host`.
    pub host: String,
    /// CA TCP port.
    pub port: u16,
    /// TXT records (key=value).
    pub txt: Vec<(String, String)>,
    /// TTL on every record we add.
    pub ttl: Duration,
    /// How often to refresh the records to keep them fresh.
    pub keepalive: Duration,
    /// Optional TSIG key for authenticated UPDATE.
    pub tsig: Option<TsigKey>,
}

impl Default for DnsRegistration {
    fn default() -> Self {
        Self {
            server: "127.0.0.1:53".parse().unwrap(),
            zone: "local.".to_string(),
            instance: "ioc".to_string(),
            host: "localhost".to_string(),
            port: 5064,
            txt: Vec::new(),
            ttl: Duration::from_secs(60),
            keepalive: Duration::from_secs(30),
            tsig: None,
        }
    }
}

/// Drop guard that owns the keepalive task and the registration
/// metadata required to send DELETE updates on shutdown.
pub struct DnsUpdater {
    shutdown_tx: Option<tokio::sync::oneshot::Sender<()>>,
    handle: Option<tokio::task::JoinHandle<()>>,
}

impl DnsUpdater {
    /// Send the initial CREATE UPDATE and spawn a keepalive loop.
    pub async fn register(reg: DnsRegistration) -> Result<Self, ClientError> {
        // Initial registration.
        send_update(&reg, UpdateOp::Create).await?;
        tracing::info!(zone = %reg.zone, instance = %reg.instance,
            server = %reg.server, "DNS UPDATE: registered");
        metrics::counter!("ca_server_dns_update_register_total").increment(1);

        let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let reg_clone = reg.clone();
        let handle = tokio::spawn(async move {
            let mut interval = tokio::time::interval(reg_clone.keepalive);
            interval.tick().await; // first tick fires immediately; skip
            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        if let Err(e) = send_update(&reg_clone, UpdateOp::Refresh).await {
                            tracing::warn!(error = %e, "DNS UPDATE refresh failed");
                            metrics::counter!("ca_server_dns_update_refresh_failures_total").increment(1);
                        }
                    }
                    _ = &mut shutdown_rx => break,
                }
            }
            // Best-effort delete on shutdown.
            if let Err(e) = send_update(&reg_clone, UpdateOp::Delete).await {
                tracing::warn!(error = %e, "DNS UPDATE delete on shutdown failed");
            } else {
                tracing::info!(zone = %reg_clone.zone, instance = %reg_clone.instance,
                    "DNS UPDATE: unregistered on shutdown");
            }
        });

        Ok(Self {
            shutdown_tx: Some(shutdown_tx),
            handle: Some(handle),
        })
    }
}

impl Drop for DnsUpdater {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        // Don't .await — Drop is sync. The keepalive task will see the
        // shutdown signal, send DELETE, then exit. Caller can hold the
        // handle to await it explicitly.
        if let Some(h) = self.handle.take() {
            // Detach.
            std::mem::drop(h);
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum UpdateOp {
    Create,
    Refresh,
    Delete,
}

async fn send_update(reg: &DnsRegistration, op: UpdateOp) -> Result<(), ClientError> {
    let zone =
        Name::from_str(&reg.zone).map_err(|e| ClientError::from(format!("bad zone: {e}")))?;
    let svc_type = parse_or_err(&format!("_epics-ca._tcp.{}", reg.zone))?;
    let instance_fqdn = parse_or_err(&format!("{}._epics-ca._tcp.{}", reg.instance, reg.zone))?;
    let host_fqdn = if reg.host.ends_with('.') {
        parse_or_err(&reg.host)?
    } else {
        parse_or_err(&format!("{}.{}", reg.host, reg.zone))?
    };

    // Connect to the DNS server. TCP because UPDATE messages can grow.
    let (stream, sender) = TcpClientStream::<AsyncIoTokioAsStd<TokioTcpStream>>::new(reg.server);

    let signer: Option<Arc<Signer>> = match &reg.tsig {
        Some(key) => {
            let signer_name = parse_or_err(&key.name)?;
            let tsigner = TSigner::new(
                key.secret.clone(),
                key.algorithm.to_proto(),
                signer_name,
                300, // fudge: 300s skew tolerance per RFC 2845 recommendation
            )
            .map_err(|e| ClientError::from(format!("TSIG init: {e}")))?;
            Some(Arc::new(Signer::TSIG(tsigner)))
        }
        None => None,
    };

    let (mut client, bg) = AsyncClient::new(stream, sender, signer).await?;
    tokio::spawn(bg);

    let ttl = reg.ttl.as_secs() as u32;
    // SRV record
    let srv_rdata = RData::SRV(SRV::new(0, 0, reg.port, host_fqdn));
    let srv = Record::from_rdata(instance_fqdn.clone(), ttl, srv_rdata);

    // TXT record
    let txt_strs: Vec<String> = reg.txt.iter().map(|(k, v)| format!("{k}={v}")).collect();
    let txt_rdata = RData::TXT(TXT::new(txt_strs));
    let txt = Record::from_rdata(instance_fqdn.clone(), ttl, txt_rdata);

    // PTR record (service-type → instance)
    let ptr_rdata = RData::PTR(hickory_client::rr::rdata::PTR(instance_fqdn.clone()));
    let ptr = Record::from_rdata(svc_type, ttl, ptr_rdata);

    match op {
        UpdateOp::Create | UpdateOp::Refresh => {
            // Use append() with must_exist=false — replaces if present,
            // creates otherwise. RFC 2136 leaves create vs update up to
            // the server; append-with-may-exist works for both.
            client.append(srv, zone.clone(), false).await.map_err(|e| {
                tracing::debug!(error = %e, "SRV append failed");
                e
            })?;
            client.append(txt, zone.clone(), false).await?;
            client.append(ptr, zone, false).await?;
        }
        UpdateOp::Delete => {
            // delete_by_rdata removes the specific record we know we added.
            let _ = client.delete_by_rdata(srv.clone(), zone.clone()).await;
            let _ = client.delete_by_rdata(txt, zone.clone()).await;
            let _ = client.delete_by_rdata(ptr, zone).await;
        }
    }
    Ok(())
}

fn parse_or_err(s: &str) -> Result<Name, ClientError> {
    Name::from_str(s).map_err(|e| ClientError::from(format!("bad name {s:?}: {e}")))
}
