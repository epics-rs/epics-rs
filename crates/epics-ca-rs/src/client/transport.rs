use std::collections::HashMap;
use std::net::SocketAddr;
#[cfg(feature = "experimental-rust-tls")]
use std::sync::Arc;
use std::time::Duration;

use epics_base_rs::runtime::sync::mpsc;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::channel::AccessRights;
use crate::protocol::*;

use super::types::{TransportCommand, TransportEvent};

/// Optional client-side TLS handshaker. `None` means plaintext.
/// Behind the `tls` feature so default builds carry zero TLS code.
#[cfg(feature = "experimental-rust-tls")]
type TlsConnector = tokio_rustls::TlsConnector;
#[cfg(feature = "experimental-rust-tls")]
type ClientTlsConfig = Arc<tokio_rustls::rustls::ClientConfig>;

/// Timeout for echo response before declaring connection dead (matches C EPICS CA_ECHO_TIMEOUT).
const ECHO_TIMEOUT_SECS: u64 = 5;

/// Maximum accumulated TCP read buffer before disconnecting.
/// Protects against malformed servers declaring huge payloads.
const MAX_ACCUMULATED: usize = 1024 * 1024; // 1 MB

/// Send buffer backpressure threshold (matches C EPICS flushBlockThreshold).
/// If more than this many frames are pending, the connection is stalled.
const SEND_BACKPRESSURE_FRAMES: usize = 4096;

/// Default echo interval in seconds (matches C EPICS CA_CONN_VERIFY_PERIOD).
/// Overridden by EPICS_CA_CONN_TMO environment variable.
fn echo_idle_secs() -> u64 {
    epics_base_rs::runtime::env::get("EPICS_CA_CONN_TMO")
        .and_then(|s| s.parse::<f64>().ok())
        .map(|v| v.max(1.0) as u64)
        .unwrap_or(30)
}

struct ServerConnection {
    write_tx: mpsc::UnboundedSender<Vec<u8>>,
    pending_frames: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    /// Beacon-arrival channel into `read_loop`. `false` = healthy
    /// beacon (refresh idle watchdog deadline); `true` = anomaly
    /// classified by `beacon_monitor` (set the in-loop flag so
    /// subsequent healthy beacons don't refresh the deadline either,
    /// causing the watchdog to expire on schedule and probe the
    /// circuit then). Mirrors libca's `tcpRecvWatchdog` model — see
    /// `TransportCommand::BeaconArrivalNotify` for full rationale.
    beacon_arrival_tx: mpsc::UnboundedSender<bool>,
    _read_task: tokio::task::JoinHandle<()>,
    _write_task: tokio::task::JoinHandle<()>,
}

/// Per-task transport manager.
///
/// `in_flight` is the Option-C Phase-A shared in-flight read/write
/// registry: each spawned per-server `read_loop` gets a clone so it
/// can dispatch `READ_NOTIFY` / `WRITE_NOTIFY` responses straight to
/// the originating caller's oneshot, without a coordinator hop.
///
/// `last_rx_at` is the per-server "last frame received" sidecar
/// (Option C, Phase D): the read loop bumps it on every TCP frame
/// so `ca_receive_watchdog_delay` stays accurate even for read-only
/// or write-only workloads whose responses no longer reach the
/// coordinator.
pub(crate) async fn run_transport_manager(
    mut command_rx: mpsc::UnboundedReceiver<TransportCommand>,
    event_tx: mpsc::UnboundedSender<TransportEvent>,
    in_flight: super::types::InFlightOps,
    last_rx_at: super::types::ServerLastRxAt,
    #[cfg(feature = "experimental-rust-tls")] tls: Option<ClientTlsConfig>,
    #[cfg(feature = "experimental-rust-tls")] tls_server_name: Option<String>,
    // Per-server SNI / cert-verification overrides built from the
    // hostname half of EPICS_CA_NAME_SERVERS=host:port entries.
    // Looked up per connect_server call so each TLS handshake uses
    // the operator-supplied DNS name for that specific peer; falls
    // back to tls_server_name (the global override), then the IP
    // literal. Empty when no name servers were given by hostname.
    #[cfg(feature = "experimental-rust-tls")] sni_overrides: HashMap<SocketAddr, String>,
) {
    let mut connections: HashMap<SocketAddr, ServerConnection> = HashMap::new();
    // Pending connect_server tasks. Spawning each connect into a
    // JoinSet (rather than `.await`-ing inline) is what lets a
    // slow TCP/TLS handshake on server A stop blocking unrelated
    // commands: BeaconArrivalNotify for already-connected
    // circuits, fast-path CreateChannel for server B, etc. The
    // task returns its `server_addr` alongside the result so
    // `join_next` can pair completion with the right state.
    let mut pending_connects: tokio::task::JoinSet<(SocketAddr, Option<ServerConnection>)> =
        tokio::task::JoinSet::new();
    // Commands waiting on a pending connect. Keyed by the
    // command's target server. CreateChannel is the only command
    // that *causes* a connect to start; subsequent CreateChannels
    // for the same server (and any non-CreateChannel commands that
    // happen to arrive before connect completes) all queue here
    // and get drained when the connect resolves.
    let mut queued_per_server: HashMap<SocketAddr, Vec<TransportCommand>> = HashMap::new();

    // Helper: resolve the right SNI / cert-verification name for a
    // particular target address. Lookup order:
    //   1. Exact (ip:port) match — EPICS_CA_NAME_SERVERS hostname or
    //      EPICS_CA_TLS_SNI_MAP "ip:port=host" entry.
    //   2. Wildcard (ip:0) match — EPICS_CA_TLS_SNI_MAP "ip=host"
    //      entry (any port). F-G6: lets operators map an IOC's IP
    //      once and have it apply to every port the search engine
    //      finds it on.
    //   3. Global EPICS_CA_TLS_SERVER_NAME fallback.
    //   4. (Caller's last fallback) IP literal as SNI.
    #[cfg(feature = "experimental-rust-tls")]
    let pick_sni = |addr: SocketAddr| -> Option<String> {
        if let Some(h) = sni_overrides.get(&addr) {
            return Some(h.clone());
        }
        let wildcard = SocketAddr::new(addr.ip(), 0);
        if let Some(h) = sni_overrides.get(&wildcard) {
            return Some(h.clone());
        }
        tls_server_name.clone()
    };

    loop {
        tokio::select! {
            cmd = command_rx.recv() => {
                let Some(cmd) = cmd else { return };
                let server_addr = cmd_server_addr(&cmd);

                // If a connect to this server is already in
                // flight, queue. Per-server FIFO is preserved
                // because we push at the tail and drain at
                // completion.
                if queued_per_server.contains_key(&server_addr) {
                    queued_per_server
                        .get_mut(&server_addr)
                        .expect("just checked contains_key")
                        .push(cmd);
                    continue;
                }

                // Only CreateChannel triggers a connect. Other
                // commands either find the connection already
                // present or silently no-op via send_frame, which
                // matches pre-refactor behaviour for the rare
                // case where a command races a circuit teardown.
                if matches!(&cmd, TransportCommand::CreateChannel { .. }) {
                    let alive = connections
                        .get(&server_addr)
                        .map(|c| !c._read_task.is_finished() && !c._write_task.is_finished())
                        .unwrap_or(false);
                    if !alive {
                        // Either no connection at all, or a
                        // stale entry whose tasks are already
                        // dead. Abort the dead pair before
                        // spawning a fresh connect.
                        if let Some(old) = connections.remove(&server_addr) {
                            old._read_task.abort();
                            old._write_task.abort();
                        }
                        let event_tx_clone = event_tx.clone();
                        #[cfg(feature = "experimental-rust-tls")]
                        let tls_clone = tls.clone();
                        #[cfg(feature = "experimental-rust-tls")]
                        let sni = pick_sni(server_addr);
                        let in_flight_clone = in_flight.clone();
                        let last_rx_clone = last_rx_at.clone();
                        pending_connects.spawn(async move {
                            #[cfg(feature = "experimental-rust-tls")]
                            let conn = connect_server(
                                server_addr,
                                event_tx_clone,
                                in_flight_clone,
                                last_rx_clone,
                                tls_clone.as_ref(),
                                sni.as_deref(),
                            )
                            .await;
                            #[cfg(not(feature = "experimental-rust-tls"))]
                            let conn = connect_server(
                                server_addr,
                                event_tx_clone,
                                in_flight_clone,
                                last_rx_clone,
                            )
                            .await;
                            (server_addr, conn)
                        });
                        // Queue this CreateChannel so its
                        // CREATE_CHAN frame goes out once the
                        // connection is up. Subsequent commands
                        // for this server will hit the
                        // `queued_per_server.contains_key` guard
                        // above and join the same queue.
                        queued_per_server.insert(server_addr, vec![cmd]);
                        continue;
                    }
                }

                process_command(cmd, &mut connections, &event_tx);
            }
            Some(joined) = pending_connects.join_next() => {
                let (server_addr, result) = match joined {
                    Ok(v) => v,
                    // Task panicked or was aborted before
                    // returning. Treat as "no result" — drop the
                    // queue (a panic in connect_server is a bug
                    // we can't recover from here) and continue.
                    Err(_) => continue,
                };
                let queued = queued_per_server.remove(&server_addr).unwrap_or_default();
                match result {
                    Some(conn) => {
                        connections.insert(server_addr, conn);
                        for queued_cmd in queued {
                            process_command(queued_cmd, &mut connections, &event_tx);
                        }
                    }
                    None => {
                        // Connect failed. Surface
                        // ChannelCreateFailed for each queued
                        // CreateChannel so the coordinator knows
                        // the channel can't progress on this
                        // server, and a single TcpClosed so the
                        // coordinator can clear any other state
                        // it kept on this server_addr.
                        for queued_cmd in queued {
                            if let TransportCommand::CreateChannel { cid, .. } = queued_cmd {
                                let _ = event_tx.send(TransportEvent::ChannelCreateFailed { cid });
                            }
                        }
                        let _ = event_tx.send(TransportEvent::TcpClosed { server_addr });
                    }
                }
            }
        }
    }
}

/// Extract the target server address from any `TransportCommand`.
/// Used by `run_transport_manager` to decide whether a command
/// needs to be queued behind a pending connect for that server.
fn cmd_server_addr(cmd: &TransportCommand) -> SocketAddr {
    match cmd {
        TransportCommand::CreateChannel { server_addr, .. }
        | TransportCommand::ReadNotify { server_addr, .. }
        | TransportCommand::Write { server_addr, .. }
        | TransportCommand::WriteNotify { server_addr, .. }
        | TransportCommand::Subscribe { server_addr, .. }
        | TransportCommand::Unsubscribe { server_addr, .. }
        | TransportCommand::ClearChannel { server_addr, .. }
        | TransportCommand::BeaconArrivalNotify { server_addr, .. }
        | TransportCommand::EventsOff { server_addr }
        | TransportCommand::EventsOn { server_addr } => *server_addr,
    }
}

/// Process a single command against an already-decided connection
/// state. Caller is responsible for ensuring any required connect
/// has completed (CreateChannel only — other commands rely on the
/// channel having been created successfully, which implies the
/// connection exists). All variants ultimately reduce to building
/// a CA frame and handing it to `send_frame`, except
/// `BeaconArrivalNotify` which forwards to the per-circuit
/// watchdog channel.
fn process_command(
    cmd: TransportCommand,
    connections: &mut HashMap<SocketAddr, ServerConnection>,
    event_tx: &mpsc::UnboundedSender<TransportEvent>,
) {
    match cmd {
        TransportCommand::CreateChannel {
            cid,
            pv_name,
            server_addr,
        } => {
            let pv_payload = pad_string(&pv_name);
            let mut create_hdr = CaHeader::new(CA_PROTO_CREATE_CHAN);
            create_hdr.postsize = pv_payload.len() as u16;
            create_hdr.cid = cid;
            create_hdr.available = CA_MINOR_VERSION as u32;
            let mut frame = create_hdr.to_bytes().to_vec();
            frame.extend_from_slice(&pv_payload);
            send_frame(connections, server_addr, frame, event_tx);
        }
        TransportCommand::ReadNotify {
            sid,
            data_type,
            count,
            ioid,
            server_addr,
        } => {
            let mut hdr = CaHeader::new(CA_PROTO_READ_NOTIFY);
            hdr.data_type = data_type;
            hdr.cid = sid;
            hdr.available = ioid;
            if count > 0xFFFF {
                hdr.set_payload_size(0, count);
            } else {
                hdr.count = count as u16;
            }
            send_frame(connections, server_addr, hdr.to_bytes_extended(), event_tx);
        }
        TransportCommand::Write {
            sid,
            data_type,
            count,
            payload,
            server_addr,
        } => {
            let padded_len = align8(payload.len());
            let mut padded = payload;
            padded.resize(padded_len, 0);

            let mut hdr = CaHeader::new(CA_PROTO_WRITE);
            hdr.data_type = data_type;
            hdr.cid = sid;
            hdr.set_payload_size(padded.len(), count);

            let mut frame = hdr.to_bytes_extended();
            frame.extend_from_slice(&padded);
            send_frame(connections, server_addr, frame, event_tx);
        }
        TransportCommand::WriteNotify {
            sid,
            data_type,
            count,
            ioid,
            payload,
            server_addr,
        } => {
            let padded_len = align8(payload.len());
            let mut padded = payload;
            padded.resize(padded_len, 0);

            let mut hdr = CaHeader::new(CA_PROTO_WRITE_NOTIFY);
            hdr.data_type = data_type;
            hdr.cid = sid;
            hdr.available = ioid;
            hdr.set_payload_size(padded.len(), count);

            let mut frame = hdr.to_bytes_extended();
            frame.extend_from_slice(&padded);
            send_frame(connections, server_addr, frame, event_tx);
        }
        TransportCommand::Subscribe {
            sid,
            data_type,
            count,
            subid,
            mask,
            server_addr,
        } => {
            let mut hdr = CaHeader::new(CA_PROTO_EVENT_ADD);
            hdr.postsize = 16;
            hdr.data_type = data_type;
            hdr.cid = sid;
            hdr.available = subid;
            if count > 0xFFFF {
                hdr.set_payload_size(16, count);
            } else {
                hdr.count = count as u16;
            }

            let mut mask_payload = [0u8; 16];
            mask_payload[12..14].copy_from_slice(&mask.to_be_bytes());

            let mut frame = hdr.to_bytes_extended();
            frame.extend_from_slice(&mask_payload);
            send_frame(connections, server_addr, frame, event_tx);
        }
        TransportCommand::Unsubscribe {
            sid,
            subid,
            data_type,
            server_addr,
        } => {
            let mut hdr = CaHeader::new(CA_PROTO_EVENT_CANCEL);
            hdr.data_type = data_type;
            hdr.cid = sid;
            hdr.available = subid;
            send_frame(connections, server_addr, hdr.to_bytes().to_vec(), event_tx);
        }
        TransportCommand::ClearChannel {
            cid,
            sid,
            server_addr,
        } => {
            let mut hdr = CaHeader::new(CA_PROTO_CLEAR_CHANNEL);
            hdr.cid = sid;
            hdr.available = cid;
            send_frame(connections, server_addr, hdr.to_bytes().to_vec(), event_tx);
        }
        TransportCommand::BeaconArrivalNotify {
            server_addr,
            anomaly,
        } => {
            // Forward the beacon classification to the per-circuit
            // read loop. Healthy beacons refresh the watchdog
            // deadline (libca `beaconArrivalNotify`); anomaly
            // beacons set a sticky flag (libca
            // `beaconAnomalyNotify`) so the watchdog expires on
            // its own schedule and probes the circuit then,
            // rather than firing an immediate probe under load.
            if let Some(conn) = connections.get(&server_addr) {
                let _ = conn.beacon_arrival_tx.send(anomaly);
            }
        }
        TransportCommand::EventsOff { server_addr } => {
            let hdr = CaHeader::new(CA_PROTO_EVENTS_OFF);
            send_frame(connections, server_addr, hdr.to_bytes().to_vec(), event_tx);
        }
        TransportCommand::EventsOn { server_addr } => {
            let hdr = CaHeader::new(CA_PROTO_EVENTS_ON);
            send_frame(connections, server_addr, hdr.to_bytes().to_vec(), event_tx);
        }
    }
}

fn send_frame(
    connections: &mut HashMap<SocketAddr, ServerConnection>,
    server_addr: SocketAddr,
    frame: Vec<u8>,
    event_tx: &mpsc::UnboundedSender<TransportEvent>,
) {
    let failed = if let Some(conn) = connections.get(&server_addr) {
        let pending = conn
            .pending_frames
            .load(std::sync::atomic::Ordering::Relaxed);
        if pending >= SEND_BACKPRESSURE_FRAMES {
            eprintln!("CA: {server_addr}: send buffer stalled ({pending} frames pending), closing");
            true
        } else {
            conn.pending_frames
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            conn.write_tx.send(frame).is_err()
        }
    } else {
        false
    };
    if failed {
        connections.remove(&server_addr);
        let _ = event_tx.send(TransportEvent::TcpClosed { server_addr });
    }
}

async fn connect_server(
    server_addr: SocketAddr,
    event_tx: mpsc::UnboundedSender<TransportEvent>,
    in_flight: super::types::InFlightOps,
    last_rx_at: super::types::ServerLastRxAt,
    #[cfg(feature = "experimental-rust-tls")] tls: Option<&ClientTlsConfig>,
    #[cfg(feature = "experimental-rust-tls")] tls_server_name: Option<&str>,
) -> Option<ServerConnection> {
    tracing::debug!(server = %server_addr, "establishing TCP virtual circuit");
    let stream = match tokio::time::timeout(
        std::time::Duration::from_secs(5),
        TcpStream::connect(server_addr),
    )
    .await
    {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => {
            tracing::warn!(server = %server_addr, error = %e, "TCP connect failed");
            return None;
        }
        Err(_) => {
            tracing::warn!(server = %server_addr, "TCP connect timed out");
            return None;
        }
    };

    let _ = stream.set_nodelay(true);

    // TCP keepalive: detect dead connections on idle circuits.
    // OS sends probes after 15s idle, every 5s, giving up after 3 failures (~30s total).
    {
        let sock = socket2::SockRef::from(&stream);
        let keepalive = socket2::TcpKeepalive::new()
            .with_time(Duration::from_secs(15))
            .with_interval(Duration::from_secs(5));
        let _ = sock.set_keepalive(true);
        let _ = sock.set_tcp_keepalive(&keepalive);
    }

    let (write_tx, write_rx) = mpsc::unbounded_channel();
    let pending_frames = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (beacon_arrival_tx, beacon_arrival_rx) = mpsc::unbounded_channel::<bool>();

    // Build initial CA handshake (VERSION + HOST + CLIENT) — same on
    // both plaintext and TLS paths.
    let mut handshake = Vec::new();
    let mut version_hdr = CaHeader::new(CA_PROTO_VERSION);
    version_hdr.count = CA_MINOR_VERSION;
    handshake.extend_from_slice(&version_hdr.to_bytes());
    let hostname = epics_base_rs::runtime::env::hostname();
    let host_payload = pad_string(&hostname);
    let mut host_hdr = CaHeader::new(CA_PROTO_HOST_NAME);
    host_hdr.postsize = host_payload.len() as u16;
    handshake.extend_from_slice(&host_hdr.to_bytes());
    handshake.extend_from_slice(&host_payload);
    let username = epics_base_rs::runtime::env::get("USER")
        .or_else(|| epics_base_rs::runtime::env::get("USERNAME"))
        .unwrap_or_else(|| "unknown".to_string());
    let user_payload = pad_string(&username);
    let mut user_hdr = CaHeader::new(CA_PROTO_CLIENT_NAME);
    user_hdr.postsize = user_payload.len() as u16;
    handshake.extend_from_slice(&user_hdr.to_bytes());
    handshake.extend_from_slice(&user_payload);
    pending_frames.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let _ = write_tx.send(handshake);

    // Spawn read/write tasks. The TLS path wraps the TCP stream in a
    // `tokio_rustls::TlsStream` first; the plaintext path splits the
    // raw TcpStream. Both feed identical-shape generic loops.
    #[cfg(feature = "experimental-rust-tls")]
    let (read_task, write_task) = if let Some(tls_cfg) = tls {
        // Prefer the operator-supplied SNI / cert-hostname-verification
        // name (e.g. EPICS_CA_TLS_SERVER_NAME=ioc.example.com); fall back
        // to the server's IP literal when nothing is configured. The IP
        // literal only validates against IP-bound certs, so hostname-bound
        // certs require the explicit override.
        let sni_str: String = match tls_server_name {
            Some(n) if !n.is_empty() => n.to_owned(),
            _ => server_addr.ip().to_string(),
        };
        let server_name = match tokio_rustls::rustls::pki_types::ServerName::try_from(sni_str) {
            Ok(n) => n,
            Err(e) => {
                tracing::warn!(server = %server_addr, error = %e, "invalid TLS server name");
                return None;
            }
        };
        let connector = TlsConnector::from(tls_cfg.clone());
        // C-G13: cap the client-side TLS handshake. A misbehaving (or
        // hostile) server that completes TCP but stalls during
        // ServerHello would otherwise leave the client awaiting
        // forever. Pairs with the existing TCP-connect timeout above.
        // 10s default — long enough for a normal cert exchange, short
        // enough to fall through to the next NAME_SERVER candidate.
        let hs_timeout = epics_base_rs::runtime::env::get("EPICS_CA_TLS_HANDSHAKE_TMO")
            .and_then(|s| s.parse::<f64>().ok())
            .map(|v| Duration::from_secs_f64(v.max(1.0)))
            .unwrap_or(Duration::from_secs(10));
        let tls_stream =
            match tokio::time::timeout(hs_timeout, connector.connect(server_name, stream)).await {
                Ok(Ok(s)) => s,
                Ok(Err(e)) => {
                    tracing::warn!(server = %server_addr, error = %e, "TLS handshake failed");
                    return None;
                }
                Err(_) => {
                    tracing::warn!(server = %server_addr,
                    timeout = ?hs_timeout, "TLS handshake timed out");
                    return None;
                }
            };
        tracing::debug!(server = %server_addr, "TLS handshake complete");
        let (reader, writer) = tokio::io::split(tls_stream);
        let write_task = epics_base_rs::runtime::task::spawn(write_loop(
            writer,
            write_rx,
            server_addr,
            event_tx.clone(),
            pending_frames.clone(),
        ));
        let read_task = epics_base_rs::runtime::task::spawn(read_loop(
            reader,
            server_addr,
            event_tx,
            write_tx.clone(),
            beacon_arrival_rx,
            in_flight.clone(),
            last_rx_at.clone(),
        ));
        (read_task, write_task)
    } else {
        let (reader, writer) = stream.into_split();
        let write_task = epics_base_rs::runtime::task::spawn(write_loop(
            writer,
            write_rx,
            server_addr,
            event_tx.clone(),
            pending_frames.clone(),
        ));
        let read_task = epics_base_rs::runtime::task::spawn(read_loop(
            reader,
            server_addr,
            event_tx,
            write_tx.clone(),
            beacon_arrival_rx,
            in_flight.clone(),
            last_rx_at.clone(),
        ));
        (read_task, write_task)
    };

    #[cfg(not(feature = "experimental-rust-tls"))]
    let (read_task, write_task) = {
        let (reader, writer) = stream.into_split();
        let write_task = epics_base_rs::runtime::task::spawn(write_loop(
            writer,
            write_rx,
            server_addr,
            event_tx.clone(),
            pending_frames.clone(),
        ));
        let read_task = epics_base_rs::runtime::task::spawn(read_loop(
            reader,
            server_addr,
            event_tx,
            write_tx.clone(),
            beacon_arrival_rx,
            in_flight.clone(),
            last_rx_at.clone(),
        ));
        (read_task, write_task)
    };

    Some(ServerConnection {
        write_tx,
        pending_frames,
        beacon_arrival_tx,
        _read_task: read_task,
        _write_task: write_task,
    })
}

async fn write_loop<W: AsyncWrite + Unpin + Send + 'static>(
    mut writer: W,
    mut rx: mpsc::UnboundedReceiver<Vec<u8>>,
    server_addr: SocketAddr,
    event_tx: mpsc::UnboundedSender<TransportEvent>,
    pending_frames: std::sync::Arc<std::sync::atomic::AtomicUsize>,
) {
    // Send watchdog: if write stalls for 2x echo timeout, declare circuit dead.
    // Matches C EPICS tcpSendWatchdog behavior.
    let send_timeout = Duration::from_secs(ECHO_TIMEOUT_SECS * 2);
    let mut batch = Vec::with_capacity(4096);
    while let Some(frame) = rx.recv().await {
        let mut drained: usize = 1;
        batch.extend_from_slice(&frame);
        // Drain all pending frames into a single write
        while let Ok(frame) = rx.try_recv() {
            batch.extend_from_slice(&frame);
            drained += 1;
        }
        match tokio::time::timeout(send_timeout, writer.write_all(&batch)).await {
            Ok(Ok(())) => {
                batch.clear();
                // Saturating: read_loop also sends frames (echo, flow
                // control) that bypass send_frame's increment.
                let prev = pending_frames.load(std::sync::atomic::Ordering::Relaxed);
                pending_frames.store(
                    prev.saturating_sub(drained),
                    std::sync::atomic::Ordering::Relaxed,
                );
            }
            Ok(Err(_)) | Err(_) => {
                let _ = event_tx.send(TransportEvent::TcpClosed { server_addr });
                return;
            }
        }
    }
}

async fn read_loop<R: AsyncRead + Unpin + Send + 'static>(
    mut reader: R,
    server_addr: SocketAddr,
    event_tx: mpsc::UnboundedSender<TransportEvent>,
    write_tx: mpsc::UnboundedSender<Vec<u8>>,
    mut beacon_arrival_rx: mpsc::UnboundedReceiver<bool>,
    in_flight: super::types::InFlightOps,
    last_rx_at: super::types::ServerLastRxAt,
) {
    // Helper: emit an echo (or pre-v4.3 READ_SYNC) request. Used
    // both on idle expiry and on the first leg of an echo timeout.
    fn send_echo(
        write_tx: &mpsc::UnboundedSender<Vec<u8>>,
        server_minor_version: u16,
    ) -> Result<(), ()> {
        let cmd = if server_minor_version >= 3 {
            CA_PROTO_ECHO
        } else {
            CA_PROTO_READ_SYNC
        };
        let echo_hdr = CaHeader::new(cmd);
        write_tx.send(echo_hdr.to_bytes().to_vec()).map_err(|_| ())
    }

    let mut buf = vec![0u8; 8192];
    let mut accumulated = Vec::new();
    let idle_timeout = Duration::from_secs(echo_idle_secs());
    let echo_timeout = Duration::from_secs(ECHO_TIMEOUT_SECS);
    let mut echo_pending = false;
    // libca `tcpRecvWatchdog::beaconAnomaly` flag. Set when the
    // beacon monitor classifies a beacon as a real restart signal
    // (`IdMismatch` / `PeriodCollapse`); suppresses subsequent
    // healthy-beacon watchdog refreshes so the deadline expires on
    // its own schedule. Cleared on any data arrival from the server.
    let mut beacon_anomaly = false;
    let mut unresponsive_notified = false;
    let mut server_minor_version: u16 = 0;
    let mut beacon_rx_open = true;

    // Single long-lived `Sleep` whose deadline we mutate in place
    // via `Sleep::reset`. This is what makes the libca model
    // expressible: we can extend the watchdog deadline on healthy
    // beacons and data arrival without restarting the read future,
    // and we can leave the deadline untouched on anomaly beacons so
    // the timer still expires on its original schedule.
    let mut deadline = tokio::time::Instant::now() + idle_timeout;
    let sleep = tokio::time::sleep_until(deadline);
    tokio::pin!(sleep);

    loop {
        let n = tokio::select! {
            // No `biased;` — let tokio randomize. With three
            // branches (beacon arrival, sleep expiry, data read)
            // a fixed priority would risk starving whichever lost
            // — initially we tried `biased` favoring the beacon
            // branch and realized that under a beacon flood it
            // could starve the data path, which is exactly the
            // failure mode we wanted to avoid. tokio's default
            // randomized polling gives uniform fairness without
            // any cleverness on our part.
            arrival = beacon_arrival_rx.recv(), if beacon_rx_open => {
                match arrival {
                    Some(true) => {
                        // libca beaconAnomalyNotify: set sticky flag,
                        // do NOT touch the deadline. The watchdog
                        // will expire on schedule and probe then —
                        // matches libca's "be careful about using
                        // beacons to reset the connection time out
                        // watchdog until we have received a ping
                        // response" comment in tcpRecvWatchdog.cpp.
                        beacon_anomaly = true;
                    }
                    Some(false) => {
                        // libca beaconArrivalNotify: refresh the
                        // deadline only when we trust beacons (no
                        // anomaly outstanding) and aren't already
                        // probing.
                        if !beacon_anomaly && !echo_pending {
                            deadline = tokio::time::Instant::now() + idle_timeout;
                            sleep.as_mut().reset(deadline);
                        }
                    }
                    None => {
                        // Transport manager dropped the sender —
                        // shutdown in progress. Stop polling this
                        // branch so we don't busy-loop on Ready(None).
                        beacon_rx_open = false;
                    }
                }
                continue;
            }
            // Watchdog deadline expired.
            _ = &mut sleep => {
                if echo_pending {
                    if !unresponsive_notified {
                        let _ = event_tx.send(TransportEvent::CircuitUnresponsive { server_addr });
                        unresponsive_notified = true;
                        if send_echo(&write_tx, server_minor_version).is_err() {
                            let _ = event_tx.send(TransportEvent::TcpClosed { server_addr });
                            return;
                        }
                        deadline = tokio::time::Instant::now() + echo_timeout;
                        sleep.as_mut().reset(deadline);
                        continue;
                    }
                    // Second echo timeout — truly dead.
                    let _ = event_tx.send(TransportEvent::TcpClosed { server_addr });
                    return;
                }
                // Idle expired — send echo heartbeat. The deadline
                // path itself doesn't read `beacon_anomaly`; the
                // flag's job is upstream, in the beacon-arrival
                // branch, where it gates whether healthy beacons
                // refresh the deadline. By the time we get here on
                // an anomaly-flagged circuit, that gating has
                // already kept the deadline at its original value
                // long enough for it to expire on the schedule it
                // would have had without any beacons at all.
                if send_echo(&write_tx, server_minor_version).is_err() {
                    let _ = event_tx.send(TransportEvent::TcpClosed { server_addr });
                    return;
                }
                echo_pending = true;
                deadline = tokio::time::Instant::now() + echo_timeout;
                sleep.as_mut().reset(deadline);
                continue;
            }
            // Data from the server.
            read_result = reader.read(&mut buf) => {
                match read_result {
                    Ok(0) | Err(_) => {
                        let _ = event_tx.send(TransportEvent::TcpClosed { server_addr });
                        return;
                    }
                    Ok(n) => n,
                }
            }
        };

        // Data received — circuit is alive. Mirrors libca
        // `messageArrivalNotify`: clear flags and refresh deadline.
        echo_pending = false;
        beacon_anomaly = false;
        deadline = tokio::time::Instant::now() + idle_timeout;
        sleep.as_mut().reset(deadline);
        // Phase D: bump the per-server "last RX" stamp before any
        // protocol parsing so that even ECHO replies and frames the
        // parser later rejects still count as proof of liveness.
        // Read by `ca_receive_watchdog_delay` via the coordinator.
        last_rx_at.insert(server_addr, std::time::Instant::now());
        if unresponsive_notified {
            unresponsive_notified = false;
            let _ = event_tx.send(TransportEvent::CircuitResponsive { server_addr });
        }

        // Automatic CA flow control is intentionally disabled here. The
        // previous implementation counted TCP reads, which can overshoot badly
        // on fragmented links and stall remote C IOCs with EVENTS_OFF. A
        // correct implementation must count parsed monitor messages and resume
        // based on downstream consumption, not socket read timing.
        accumulated.extend_from_slice(&buf[..n]);

        // Guard against unbounded buffer growth from malformed servers.
        if accumulated.len() > MAX_ACCUMULATED {
            eprintln!(
                "CA: {server_addr}: accumulated TCP buffer exceeded {} bytes, closing",
                MAX_ACCUMULATED
            );
            let _ = event_tx.send(TransportEvent::TcpClosed { server_addr });
            return;
        }

        let mut offset = 0;
        while offset + CaHeader::SIZE <= accumulated.len() {
            let (hdr, hdr_size) = match CaHeader::from_bytes_extended(&accumulated[offset..]) {
                Ok(v) => v,
                Err(_) => {
                    eprintln!("CA: {server_addr}: malformed TCP header, skipping");
                    break;
                }
            };
            let actual_post = hdr.actual_postsize();
            let msg_len = hdr_size + align8(actual_post);

            if offset + msg_len > accumulated.len() {
                break;
            }

            let data_start = offset + hdr_size;
            let data_end = data_start + actual_post;

            // Defense-in-depth: verify payload is within buffer bounds
            // even though msg_len check above should guarantee this.
            if data_end > accumulated.len() {
                eprintln!("CA: {server_addr}: payload exceeds buffer bounds, skipping");
                break;
            }

            match hdr.cmmd {
                CA_PROTO_VERSION => {
                    server_minor_version = hdr.count;
                    let _ = event_tx.send(TransportEvent::ServerVersion {
                        server_addr,
                        minor_version: hdr.count,
                    });
                }
                CA_PROTO_ACCESS_RIGHTS => {
                    let _ = event_tx.send(TransportEvent::AccessRightsChanged {
                        cid: hdr.cid,
                        access: AccessRights::from_u32(hdr.available),
                    });
                }
                CA_PROTO_CREATE_CHAN => {
                    let _ = event_tx.send(TransportEvent::ChannelCreated {
                        cid: hdr.cid,
                        sid: hdr.available,
                        data_type: hdr.data_type,
                        element_count: hdr.actual_count(),
                        access: AccessRights::from_u32(0x3),
                        server_addr,
                    });
                }
                CA_PROTO_READ_NOTIFY => {
                    // Direct dispatch to the in-flight read registry
                    // (Option C Phase A) — bypasses the coordinator's
                    // `tokio::select!` loop. The originating
                    // `ch.get()` task is awaiting the oneshot we
                    // resolve here.
                    let ioid = hdr.available;
                    if hdr.cid == ECA_NORMAL {
                        let data = accumulated[data_start..data_start + actual_post].to_vec();
                        if let Some((_, (_, reply_tx))) = in_flight.reads.remove(&ioid) {
                            let _ = reply_tx.send(Ok((
                                hdr.data_type,
                                hdr.actual_count(),
                                data,
                            )));
                        }
                    } else if let Some((_, (_, reply_tx))) = in_flight.reads.remove(&ioid) {
                        let _ = reply_tx.send(Err(epics_base_rs::error::CaError::Protocol(
                            format!("server returned ECA error {:#06x}", hdr.cid),
                        )));
                    }
                }
                CA_PROTO_WRITE_NOTIFY => {
                    // Direct dispatch to the in-flight write registry
                    // (Option C Phase A). Mirrors the read path: the
                    // originating `ch.put()` task is awaiting the
                    // oneshot we resolve here. `hdr.cid` carries the
                    // ECA status — `1` (`ECA_NORMAL`) means success;
                    // anything else is mapped to `CaError::WriteFailed`.
                    let ioid = hdr.available;
                    let status = hdr.cid;
                    if let Some((_, (_, reply_tx))) = in_flight.writes.remove(&ioid) {
                        if status == 1 || status == ECA_NORMAL {
                            let _ = reply_tx.send(Ok(()));
                        } else {
                            let _ = reply_tx
                                .send(Err(epics_base_rs::error::CaError::WriteFailed(status)));
                        }
                    }
                }
                CA_PROTO_EVENT_ADD => {
                    let data = accumulated[data_start..data_start + actual_post].to_vec();
                    let _ = event_tx.send(TransportEvent::MonitorData {
                        subid: hdr.available,
                        data_type: hdr.data_type,
                        count: hdr.actual_count(),
                        data,
                    });
                }
                CA_PROTO_ECHO | CA_PROTO_READ_SYNC => {
                    // Echo response from server — liveness already handled
                    // above (echo_pending=false).  Do NOT echo back; only
                    // the server echoes requests.  Responding here would
                    // create a tight ping-pong loop.
                }
                CA_PROTO_CREATE_CH_FAIL => {
                    let _ = event_tx.send(TransportEvent::ChannelCreateFailed { cid: hdr.cid });
                }
                CA_PROTO_ERROR => {
                    let orig_cmd = if actual_post >= 16 {
                        let orig_hdr_bytes = &accumulated[data_start..data_start + 16];
                        Some(u16::from_be_bytes([orig_hdr_bytes[0], orig_hdr_bytes[1]]))
                    } else {
                        None
                    };
                    let msg = if actual_post > 16 {
                        let msg_bytes = &accumulated[data_start + 16..data_start + actual_post];
                        let end = msg_bytes
                            .iter()
                            .position(|&b| b == 0)
                            .unwrap_or(msg_bytes.len());
                        String::from_utf8_lossy(&msg_bytes[..end]).to_string()
                    } else {
                        String::new()
                    };
                    // C ref: modules/ca/src/client/udpiiu.cpp:exceptionRespAction —
                    // commit a352865 routes the error prefix through ERL_ERROR
                    // (ANSI-colored "Error:" on TTYs). The Rust equivalent is
                    // tracing::error! which honors the configured subscriber's
                    // formatting (color, prefix, structured fields).
                    tracing::error!(
                        server = %server_addr,
                        cmd = ?orig_cmd,
                        msg = %msg,
                        "CA server error",
                    );
                    let _ = event_tx.send(TransportEvent::ServerError {
                        _original_request: orig_cmd,
                        _message: msg,
                    });
                }
                CA_PROTO_SERVER_DISCONN => {
                    let _ = event_tx.send(TransportEvent::ServerDisconnect {
                        cid: hdr.cid,
                        server_addr,
                    });
                }
                _ => {}
            }

            offset += msg_len;
        }

        if offset > 0 {
            accumulated.drain(..offset);
        }
    }
}

#[cfg(test)]
mod read_loop_tests {
    //! Virtual-time tests for the libca-style lazy-echo watchdog.
    //!
    //! `tokio::test(start_paused = true)` gives us a paused clock
    //! that auto-advances whenever all tasks are pending on time.
    //! That makes the deadline arithmetic deterministic: we can
    //! sleep the test thread to a specific virtual instant, inject
    //! beacon-arrival or data events, and assert what the read loop
    //! has produced by that point — without actual wall-clock
    //! waits that would make the test suite slow and flaky.
    //!
    //! All three tests assume the default `EPICS_CA_CONN_TMO` of 30
    //! seconds (echo_idle_secs). Tests do not set the env var to
    //! avoid cross-test contamination.
    use super::*;
    use tokio::io::AsyncWriteExt;

    fn test_addr() -> SocketAddr {
        "127.0.0.1:5064".parse().unwrap()
    }

    /// Spin up a read loop wired to a duplex pipe (so the test can
    /// drive the "server" end), an event channel, a frame channel
    /// (where the read loop's outgoing echo requests land), and a
    /// beacon-arrival channel. Returns the handles the test needs.
    fn spawn_read_loop() -> (
        tokio::io::DuplexStream,                 // server end of pipe
        mpsc::UnboundedReceiver<TransportEvent>, // events emitted
        mpsc::UnboundedReceiver<Vec<u8>>,        // frames the loop wrote
        mpsc::UnboundedSender<bool>,             // beacon arrival sender
        tokio::task::JoinHandle<()>,             // the loop task
    ) {
        let (server_end, client_end) = tokio::io::duplex(8192);
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let (write_tx, write_rx) = mpsc::unbounded_channel();
        let (beacon_tx, beacon_rx) = mpsc::unbounded_channel::<bool>();
        let task = tokio::spawn(read_loop(
            client_end,
            test_addr(),
            event_tx,
            write_tx,
            beacon_rx,
            crate::client::types::InFlightOps::new(),
            std::sync::Arc::new(dashmap::DashMap::new()),
        ));
        (server_end, event_rx, write_rx, beacon_tx, task)
    }

    /// Healthy beacon arriving partway through the idle window
    /// pushes the deadline forward (libca `beaconArrivalNotify`).
    /// Without the refresh, the loop would echo at t=30 s; with
    /// the refresh at t=20 s, the new deadline is t=50 s and no
    /// echo fires before then.
    #[tokio::test(start_paused = true)]
    async fn healthy_beacon_extends_idle_deadline() {
        let (_server_end, mut events, mut writes, beacon_tx, task) = spawn_read_loop();

        // Yield once so the spawned read_loop is actually running
        // before we start manipulating time. Without this, the
        // first `sleep` below races the spawn.
        tokio::task::yield_now().await;

        // Advance to t=20 s and push a healthy beacon. Idle
        // deadline was 30 s; after the beacon it becomes 50 s.
        tokio::time::sleep(Duration::from_secs(20)).await;
        beacon_tx.send(false).expect("beacon channel alive");

        // Advance to t=45 s (still under the new 50-s deadline).
        // No echo must have fired yet.
        tokio::time::sleep(Duration::from_secs(25)).await;
        assert!(
            writes.try_recv().is_err(),
            "healthy beacon should have extended the idle deadline past 30 s"
        );

        // Advance past t=50 s. Now the (refreshed) idle deadline
        // has expired and the loop sent an echo.
        tokio::time::sleep(Duration::from_secs(10)).await;
        let frame = writes
            .try_recv()
            .expect("echo must fire after extended deadline");
        assert_eq!(
            frame.len(),
            CaHeader::SIZE,
            "idle echo must be a bare CA header"
        );

        task.abort();
        let _ = events.try_recv();
    }

    /// Anomaly beacon sets a sticky flag (libca
    /// `beaconAnomalyNotify`); subsequent healthy beacons must
    /// NOT refresh the deadline while the flag is set. Result:
    /// the watchdog expires on its original 30-s schedule even
    /// though healthy beacons kept arriving.
    #[tokio::test(start_paused = true)]
    async fn anomaly_beacon_suppresses_healthy_refresh() {
        let (_server_end, mut events, mut writes, beacon_tx, task) = spawn_read_loop();
        tokio::task::yield_now().await;

        // Anomaly at t=5 s — flag set, deadline UNCHANGED at 30 s.
        tokio::time::sleep(Duration::from_secs(5)).await;
        beacon_tx.send(true).expect("alive");

        // Spurious healthy beacons at t=10, t=20 — must not
        // refresh because the flag is sticky.
        tokio::time::sleep(Duration::from_secs(5)).await;
        beacon_tx.send(false).expect("alive");
        tokio::time::sleep(Duration::from_secs(10)).await;
        beacon_tx.send(false).expect("alive");

        // Advance to t=31 s — past the original 30-s deadline.
        // Echo must have fired exactly because the flag prevented
        // any refresh. (Ordering of the previous beacon sends:
        // they're all consumed before time advances past 30 s,
        // because tokio polls tasks until pending before advancing.)
        tokio::time::sleep(Duration::from_secs(11)).await;
        let frame = writes
            .try_recv()
            .expect("anomaly flag must let watchdog expire on original schedule");
        assert_eq!(frame.len(), CaHeader::SIZE);

        task.abort();
        let _ = events.try_recv();
    }

    /// Data arrival from the server (libca `messageArrivalNotify`)
    /// clears both `echo_pending` and `beacon_anomaly`, and
    /// refreshes the deadline. After clearing, healthy beacons can
    /// once again refresh.
    #[tokio::test(start_paused = true)]
    async fn data_arrival_clears_anomaly_flag_and_resumes_refresh() {
        let (mut server_end, mut events, mut writes, beacon_tx, task) = spawn_read_loop();
        tokio::task::yield_now().await;

        // Anomaly at t=5 s.
        tokio::time::sleep(Duration::from_secs(5)).await;
        beacon_tx.send(true).expect("alive");

        // Server sends a CA_PROTO_VERSION frame at t=10 s. This
        // is real data → flag clears, deadline pushed to 10+30=40.
        tokio::time::sleep(Duration::from_secs(5)).await;
        let mut version_hdr = CaHeader::new(CA_PROTO_VERSION);
        version_hdr.count = 13; // some minor version
        server_end
            .write_all(&version_hdr.to_bytes())
            .await
            .expect("server end write");

        // Confirm read_loop picked up the version event. This is
        // also the moment the flag clears.
        let event = tokio::time::timeout(Duration::from_secs(1), events.recv())
            .await
            .expect("ServerVersion within 1 s")
            .expect("not closed");
        match event {
            TransportEvent::ServerVersion { minor_version, .. } => {
                assert_eq!(minor_version, 13);
            }
            _ => panic!("expected ServerVersion event"),
        }

        // Healthy beacon at t=15 s — flag is cleared so this
        // refreshes the deadline to 15+30=45.
        tokio::time::sleep(Duration::from_secs(5)).await;
        beacon_tx.send(false).expect("alive");

        // Advance to t=42 s (still under 45). No echo yet.
        tokio::time::sleep(Duration::from_secs(27)).await;
        assert!(
            writes.try_recv().is_err(),
            "post-data-arrival healthy beacon must refresh the deadline"
        );

        // Advance to t=46 s — past the refreshed deadline.
        tokio::time::sleep(Duration::from_secs(4)).await;
        let frame = writes
            .try_recv()
            .expect("echo fires once the refreshed deadline expires");
        assert_eq!(frame.len(), CaHeader::SIZE);

        task.abort();
    }
}
