use std::collections::HashMap;
use std::net::SocketAddr;
#[cfg(feature = "experimental-rust-tls")]
use std::sync::Arc;
use std::time::Duration;

use epics_base_rs::runtime::sync::mpsc;
use epics_base_rs::types::{DbFieldType, EpicsValue};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::channel::AccessRights;
use crate::protocol::*;

use super::types::{
    CircuitKey, DirectServerWriter, DirectServerWriters, InFlightOps, ReadReply, ReadReplyMode,
    ReadWaiter, SEND_BACKPRESSURE_FRAMES, TransportCommand, TransportEvent, WarmReplySlot,
};

fn dispatch_read_reply_with<F>(in_flight: &InFlightOps, ioid: u32, make_result: F)
where
    F: FnOnce(ReadReplyMode) -> epics_base_rs::error::CaResult<ReadReply>,
{
    // Hot path: warm waiter — peek the entry, take the Sender from its
    // slot, leave the entry in place so the next call on the same
    // channel can reuse this ioid without going through `alloc_ioid` +
    // DashMap insert/remove. Cold path: one-shot waiter, removed on
    // dispatch as before.
    //
    // Two DashMap touches on the cold path (1 read-lock `get` + 1
    // write-lock `remove`) instead of one — accepted because the
    // single-`get` cold path is network-bound (~70µs warm) and the
    // bulk-read hot path (this `Warm` branch) is what saves ~2µs/PV.
    let warm: Option<(ReadReplyMode, WarmReplySlot)> = {
        if let Some(entry) = in_flight.reads.get(&ioid) {
            match &*entry {
                ReadWaiter::Warm { mode, slot, .. } => Some((*mode, slot.clone())),
                ReadWaiter::OneShot { .. } => None,
            }
        } else {
            None
        }
    };
    if let Some((mode, slot)) = warm {
        let result = make_result(mode);
        if let Some(tx) = slot.lock().take() {
            let _ = tx.send(result);
        }
        return;
    }
    if let Some((_, waiter)) = in_flight.reads.remove(&ioid) {
        let result = make_result(waiter.mode());
        waiter.send(result);
    }
}

fn make_read_reply(
    mode: ReadReplyMode,
    data_type: u16,
    count: u32,
    data: &[u8],
) -> epics_base_rs::error::CaResult<ReadReply> {
    if matches!(mode, ReadReplyMode::Plain) && count == 1 {
        let dbr_type = DbFieldType::from_u16(data_type)?;
        let value = EpicsValue::from_bytes_array(dbr_type, data, count as usize)?;
        Ok(ReadReply::Plain { dbr_type, value })
    } else {
        Ok(ReadReply::Raw {
            data_type,
            count,
            data: data.to_vec(),
        })
    }
}

fn dispatch_read_error(in_flight: &InFlightOps, ioid: u32, error: epics_base_rs::error::CaError) {
    dispatch_read_reply_with(in_flight, ioid, |_| Err(error));
}

/// Optional client-side TLS handshaker. `None` means plaintext.
/// Behind the `tls` feature so default builds carry zero TLS code.
#[cfg(feature = "experimental-rust-tls")]
type TlsConnector = tokio_rustls::TlsConnector;
#[cfg(feature = "experimental-rust-tls")]
type ClientTlsConfig = Arc<tokio_rustls::rustls::ClientConfig>;

/// Timeout for echo response before declaring connection dead (matches C EPICS CA_ECHO_TIMEOUT).
const ECHO_TIMEOUT_SECS: u64 = 5;

/// Maximum accumulated TCP read buffer before disconnecting.
///
/// This MUST be >= the largest legal single frame, otherwise a valid
/// large waveform (e.g. a 2 MB array, well under the 16 MB
/// `max_payload_size()` default) sent by a server would push
/// `accumulated` past the cap and the connection would be closed
/// before the frame could be parsed — a permanent failure that
/// survives reconnect (the server re-sends, the client closes again).
///
/// Largest legal frame = extended header (24 bytes) + `max_payload_size()`
/// payload. A 64 KiB slack covers a partially-received next frame
/// pipelined behind a full one in the same read burst. `max_payload_size()`
/// honours `EPICS_CA_MAX_ARRAY_BYTES`, so the cap tracks operator overrides.
/// Mirrors the server-side cap in `server/tcp.rs`.
fn max_accumulated() -> usize {
    crate::protocol::max_payload_size()
        .saturating_add(24)
        .saturating_add(64 * 1024)
}

/// Default echo interval (matches C EPICS CA_CONN_VERIFY_PERIOD).
/// Overridden by EPICS_CA_CONN_TMO environment variable.
///
/// C `cac.cpp:186-194` parses CONN_TMO as `double` and falls
/// back to the default (30 s) on parse failure, on `<= 0.0`, AND on
/// any value libca's bookkeeping treats as a sentinel for "use the
/// default". Pre-fix Rust used `.max(1.0) as u64` which (a) rounded
/// any positive sub-second value up to 1 s (`0.5` → 1) instead of
/// honouring it verbatim, (b) truncated fractional seconds via
/// `as u64` (`15.9` → 15), and (c) clamped explicit `0` to 1 s
/// instead of falling back to the default. Match C: keep as
/// `Duration` with full sub-second precision; only `parse error`
/// or `value <= 0.0` falls back to the default.
fn echo_idle() -> Duration {
    epics_base_rs::runtime::env::get("EPICS_CA_CONN_TMO")
        .and_then(|s| s.parse::<f64>().ok())
        .filter(|v| *v > 0.0)
        .map(Duration::from_secs_f64)
        .unwrap_or(Duration::from_secs(30))
}
/// Legacy seconds accessor kept for call sites that need a coarse
/// number (e.g. `tokio::time::sleep(Duration::from_secs(N))` over a
/// long interval where sub-second precision does not matter). New
/// timer code should call `echo_idle()` directly.
fn echo_idle_secs() -> u64 {
    let d = echo_idle();
    d.as_secs().max(1)
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

/// Hard-stop on drop: abort both the per-server read and write tasks.
/// Without this, every code path that drops a `ServerConnection` (the
/// `connections.remove` on send-buffer stall in `send_frame`, the
/// implicit HashMap drop when `run_transport_manager` returns or its
/// task is aborted) would detach the inner JoinHandles, leaving the
/// per-server read/write tasks running until process exit. The
/// `read_task` holds a clone of `write_tx` and the `pending_frames`
/// Arc, so detaching it keeps the writer alive too. The companion
/// `CaClient::Drop` only aborts the four top-level tasks
/// (`coordinator` / `search` / `transport` / `beacon`); without this
/// `impl Drop`, aborting the transport manager would not cascade to
/// the per-circuit tasks it owns.
impl Drop for ServerConnection {
    fn drop(&mut self) {
        self._read_task.abort();
        self._write_task.abort();
    }
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
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_transport_manager(
    mut command_rx: mpsc::UnboundedReceiver<TransportCommand>,
    event_tx: mpsc::UnboundedSender<TransportEvent>,
    in_flight: super::types::InFlightOps,
    server_writers: DirectServerWriters,
    last_rx_at: super::types::ServerLastRxAt,
    // Shared client identity (user / host). Cloned per connect so each
    // new circuit handshakes with the value current at connect time.
    client_identity: super::types::ClientIdentitySlot,
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
    // circuits are keyed by `(SocketAddr, priority)`, so two
    // channels to the same IOC at different priorities own independent
    // TCP circuits (libca `caServerID`).
    let mut connections: HashMap<CircuitKey, ServerConnection> = HashMap::new();
    // Pending connect_server tasks. Spawning each connect into a
    // JoinSet (rather than `.await`-ing inline) is what lets a
    // slow TCP/TLS handshake on circuit A stop blocking unrelated
    // commands: BeaconArrivalNotify for already-connected
    // circuits, fast-path CreateChannel for circuit B, etc. The
    // task returns its `CircuitKey` alongside the result so
    // `join_next` can pair completion with the right state.
    let mut pending_connects: tokio::task::JoinSet<(CircuitKey, Option<ServerConnection>)> =
        tokio::task::JoinSet::new();
    // Commands waiting on a pending connect. Keyed by the command's
    // target circuit. CreateChannel is the only command that *causes*
    // a connect to start; subsequent CreateChannels for the same
    // circuit (and any non-CreateChannel commands that happen to
    // arrive before connect completes) all queue here and get drained
    // when the connect resolves.
    let mut queued_per_server: HashMap<CircuitKey, Vec<TransportCommand>> = HashMap::new();

    // Helper: resolve the right SNI / cert-verification name for a
    // particular target address. Lookup order:
    //   1. Exact (ip:port) match — EPICS_CA_NAME_SERVERS hostname or
    //      EPICS_CA_TLS_SNI_MAP "ip:port=host" entry.
    //   2. Wildcard (ip:0) match — EPICS_CA_TLS_SNI_MAP "ip=host"
    //      entry (any port). lets operators map an IOC's IP
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

                // BeaconArrivalNotify is a per-server UDP signal, not
                // tied to one priority circuit — fan it out to every
                // circuit for the server immediately (process_command
                // handles the fan-out). It never starts or waits on a
                // connect, so it skips the per-circuit queue entirely.
                let Some(circuit) = cmd_circuit_key(&cmd) else {
                    process_command(cmd, &mut connections, &server_writers, &event_tx);
                    continue;
                };

                // If a connect to this circuit is already in flight,
                // queue. Per-circuit FIFO is preserved because we push
                // at the tail and drain at completion.
                if queued_per_server.contains_key(&circuit) {
                    queued_per_server
                        .get_mut(&circuit)
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
                        .get(&circuit)
                        .map(|c| !c._read_task.is_finished() && !c._write_task.is_finished())
                        .unwrap_or(false);
                    if !alive {
                        // Either no connection at all, or a
                        // stale entry whose tasks are already
                        // dead. Abort the dead pair before
                        // spawning a fresh connect.
                        if let Some(old) = connections.remove(&circuit) {
                            server_writers.remove(&circuit);
                            old._read_task.abort();
                            old._write_task.abort();
                        }
                        let (server_addr, priority) = circuit;
                        let event_tx_clone = event_tx.clone();
                        #[cfg(feature = "experimental-rust-tls")]
                        let tls_clone = tls.clone();
                        #[cfg(feature = "experimental-rust-tls")]
                        let sni = pick_sni(server_addr);
                        let in_flight_clone = in_flight.clone();
                        let last_rx_clone = last_rx_at.clone();
                        let identity_clone = client_identity.clone();
                        pending_connects.spawn(async move {
                            #[cfg(feature = "experimental-rust-tls")]
                            let conn = connect_server(
                                server_addr,
                                priority,
                                event_tx_clone,
                                in_flight_clone,
                                last_rx_clone,
                                identity_clone,
                                tls_clone.as_ref(),
                                sni.as_deref(),
                            )
                            .await;
                            #[cfg(not(feature = "experimental-rust-tls"))]
                            let conn = connect_server(
                                server_addr,
                                priority,
                                event_tx_clone,
                                in_flight_clone,
                                last_rx_clone,
                                identity_clone,
                            )
                            .await;
                            (circuit, conn)
                        });
                        // Queue this CreateChannel so its
                        // CREATE_CHAN frame goes out once the
                        // connection is up. Subsequent commands
                        // for this circuit will hit the
                        // `queued_per_server.contains_key` guard
                        // above and join the same queue.
                        queued_per_server.insert(circuit, vec![cmd]);
                        continue;
                    }
                }

                process_command(cmd, &mut connections, &server_writers, &event_tx);
            }
            Some(joined) = pending_connects.join_next() => {
                let (circuit, result) = match joined {
                    Ok(v) => v,
                    // Task panicked or was aborted before
                    // returning. Treat as "no result" — drop the
                    // queue (a panic in connect_server is a bug
                    // we can't recover from here) and continue.
                    Err(_) => continue,
                };
                let (server_addr, priority) = circuit;
                let queued = queued_per_server.remove(&circuit).unwrap_or_default();
                match result {
                    Some(conn) => {
                        server_writers.insert(
                            circuit,
                            DirectServerWriter {
                                write_tx: conn.write_tx.clone(),
                                pending_frames: conn.pending_frames.clone(),
                            },
                        );
                        connections.insert(circuit, conn);
                        // libca bhe-on-connect parity: announce the
                        // fresh circuit so the coordinator can ask the
                        // beacon monitor to reset its per-server EMA.
                        // Emit BEFORE replaying queued commands so the
                        // reset is observed before any subsequent
                        // anomaly classification on this circuit.
                        let _ = event_tx.send(TransportEvent::ServerConnected { server_addr });
                        for queued_cmd in queued {
                            process_command(
                                queued_cmd,
                                &mut connections,
                                &server_writers,
                                &event_tx,
                            );
                        }
                    }
                    None => {
                        server_writers.remove(&circuit);
                        // Connect failed. Surface
                        // ChannelCreateFailed for each queued
                        // CreateChannel so the coordinator knows
                        // the channel can't progress on this
                        // circuit, and a single TcpClosed so the
                        // coordinator can clear any other state
                        // it kept on this circuit.
                        for queued_cmd in queued {
                            if let TransportCommand::CreateChannel { cid, .. } = queued_cmd {
                                let _ = event_tx.send(TransportEvent::ChannelCreateFailed { cid });
                            }
                        }
                        let _ = event_tx.send(TransportEvent::TcpClosed { server_addr, priority });
                    }
                }
            }
        }
    }
}

/// Extract the target virtual-circuit key `(server_addr, priority)` from
/// any `TransportCommand`. Used by `run_transport_manager` to decide
/// whether a command needs to be queued behind a pending connect for
/// that circuit.
///
/// Returns `None` for `BeaconArrivalNotify`, which is a per-server UDP
/// signal that fans out to every priority circuit for the server rather
/// than targeting one — see the main loop and `process_command`.
fn cmd_circuit_key(cmd: &TransportCommand) -> Option<CircuitKey> {
    match cmd {
        TransportCommand::CreateChannel {
            server_addr,
            priority,
            ..
        }
        | TransportCommand::ReadNotify {
            server_addr,
            priority,
            ..
        }
        | TransportCommand::Write {
            server_addr,
            priority,
            ..
        }
        | TransportCommand::WriteNotify {
            server_addr,
            priority,
            ..
        }
        | TransportCommand::Subscribe {
            server_addr,
            priority,
            ..
        }
        | TransportCommand::Unsubscribe {
            server_addr,
            priority,
            ..
        }
        | TransportCommand::ClearChannel {
            server_addr,
            priority,
            ..
        }
        | TransportCommand::EventsOff {
            server_addr,
            priority,
        }
        | TransportCommand::EventsOn {
            server_addr,
            priority,
        } => Some((*server_addr, *priority)),
        TransportCommand::BeaconArrivalNotify { .. } => None,
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
    connections: &mut HashMap<CircuitKey, ServerConnection>,
    server_writers: &DirectServerWriters,
    event_tx: &mpsc::UnboundedSender<TransportEvent>,
) {
    match cmd {
        TransportCommand::CreateChannel {
            cid,
            pv_name,
            server_addr,
            priority,
        } => {
            let pv_payload = pad_string(&pv_name);
            let mut create_hdr = CaHeader::new(CA_PROTO_CREATE_CHAN);
            create_hdr.postsize = pv_payload.len() as u16;
            create_hdr.cid = cid;
            create_hdr.available = CA_MINOR_VERSION as u32;
            let mut frame = create_hdr.to_bytes().to_vec();
            frame.extend_from_slice(&pv_payload);
            send_frame(
                connections,
                server_writers,
                (server_addr, priority),
                frame,
                event_tx,
            );
        }
        TransportCommand::ReadNotify {
            sid,
            data_type,
            count,
            ioid,
            server_addr,
            priority,
        } => {
            let mut hdr = CaHeader::new(CA_PROTO_READ_NOTIFY);
            hdr.data_type = data_type;
            hdr.cid = sid;
            hdr.available = ioid;
            // C parity (`comQueSend.cpp:285`): extended form for
            // `nElem >= 0xffff`. See `build_read_notify_frame` in
            // client/mod.rs for the same boundary in the slow path.
            if count >= 0xFFFF {
                hdr.set_payload_size(0, count);
            } else {
                hdr.count = count as u16;
            }
            send_frame(
                connections,
                server_writers,
                (server_addr, priority),
                hdr.to_bytes_extended(),
                event_tx,
            );
        }
        TransportCommand::Write {
            sid,
            data_type,
            count,
            payload,
            server_addr,
            priority,
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
            send_frame(
                connections,
                server_writers,
                (server_addr, priority),
                frame,
                event_tx,
            );
        }
        TransportCommand::WriteNotify {
            sid,
            data_type,
            count,
            ioid,
            payload,
            server_addr,
            priority,
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
            send_frame(
                connections,
                server_writers,
                (server_addr, priority),
                frame,
                event_tx,
            );
        }
        TransportCommand::Subscribe {
            sid,
            data_type,
            count,
            subid,
            mask,
            server_addr,
            priority,
        } => {
            let mut hdr = CaHeader::new(CA_PROTO_EVENT_ADD);
            hdr.postsize = 16;
            hdr.data_type = data_type;
            hdr.cid = sid;
            hdr.available = subid;
            // C parity (`comQueSend.cpp:285`): extended form for
            // `nElem >= 0xffff`. Same boundary as READ_NOTIFY above.
            if count >= 0xFFFF {
                hdr.set_payload_size(16, count);
            } else {
                hdr.count = count as u16;
            }

            let mut mask_payload = [0u8; 16];
            mask_payload[12..14].copy_from_slice(&mask.to_be_bytes());

            let mut frame = hdr.to_bytes_extended();
            frame.extend_from_slice(&mask_payload);
            send_frame(
                connections,
                server_writers,
                (server_addr, priority),
                frame,
                event_tx,
            );
        }
        TransportCommand::Unsubscribe {
            sid,
            subid,
            data_type,
            count,
            server_addr,
            priority,
        } => {
            let mut hdr = CaHeader::new(CA_PROTO_EVENT_CANCEL);
            hdr.data_type = data_type;
            // Include the subscription's original
            // count, and serialise in extended form for counts
            // >= 0xFFFF. libca
            // `tcpiiu.cpp::subscriptionCancelRequest` routes through
            // `comQueSend::insertRequestHeader` which emits the
            // extended annex automatically. Pre-fix Rust truncated
            // the count to u16 and used `to_bytes()`, so a CANCEL
            // for a >= 65,535-element monitor lost the count and
            // diverged from libca byte-for-byte.
            hdr.set_payload_size(0, count);
            hdr.cid = sid;
            hdr.available = subid;
            send_frame(
                connections,
                server_writers,
                (server_addr, priority),
                hdr.to_bytes_extended(),
                event_tx,
            );
        }
        TransportCommand::ClearChannel {
            cid,
            sid,
            server_addr,
            priority,
        } => {
            let mut hdr = CaHeader::new(CA_PROTO_CLEAR_CHANNEL);
            hdr.cid = sid;
            hdr.available = cid;
            send_frame(
                connections,
                server_writers,
                (server_addr, priority),
                hdr.to_bytes().to_vec(),
                event_tx,
            );
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
            //
            // one UDP beacon pets every priority circuit to
            // that server — fan out to all circuits whose key matches
            // `server_addr` (libca delivers `beaconArrivalNotify` to
            // each tcpiiu on the bhe's circuit list).
            for (key, conn) in connections.iter() {
                if key.0 == server_addr {
                    let _ = conn.beacon_arrival_tx.send(anomaly);
                }
            }
        }
        TransportCommand::EventsOff {
            server_addr,
            priority,
        } => {
            let hdr = CaHeader::new(CA_PROTO_EVENTS_OFF);
            send_frame(
                connections,
                server_writers,
                (server_addr, priority),
                hdr.to_bytes().to_vec(),
                event_tx,
            );
        }
        TransportCommand::EventsOn {
            server_addr,
            priority,
        } => {
            let hdr = CaHeader::new(CA_PROTO_EVENTS_ON);
            send_frame(
                connections,
                server_writers,
                (server_addr, priority),
                hdr.to_bytes().to_vec(),
                event_tx,
            );
        }
    }
}

fn send_frame(
    connections: &mut HashMap<CircuitKey, ServerConnection>,
    server_writers: &DirectServerWriters,
    circuit: CircuitKey,
    frame: Vec<u8>,
    event_tx: &mpsc::UnboundedSender<TransportEvent>,
) {
    let (server_addr, priority) = circuit;
    let failed = if let Some(conn) = connections.get(&circuit) {
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
        connections.remove(&circuit);
        server_writers.remove(&circuit);
        let _ = event_tx.send(TransportEvent::TcpClosed {
            server_addr,
            priority,
        });
    }
}

/// Build the three-frame CA client handshake (VERSION, CLIENT_NAME,
/// HOST_NAME) for a circuit at `priority`.
///
/// C `tcpiiu` constructor (`modules/ca/src/client/tcpiiu.cpp:755-762`)
/// queues messages in this exact order:
///   1. versionMessage           → CA_PROTO_VERSION
///   2. userNameSetRequest       → CA_PROTO_CLIENT_NAME
///   3. hostNameSetRequest       → CA_PROTO_HOST_NAME
///
/// Pre-fix Rust emitted VERSION → HOST_NAME → CLIENT_NAME (the last two
/// swapped). Server `host_name_action` / `client_name_action` accept
/// either order in isolation, but ACF rules that consult both fields and
/// frame-byte-exact wire captures (Wireshark CA dissector, fuzzers)
/// diverge.
///
/// the VERSION message carries the requested CA priority in its
/// `m_dataType` field — libca `tcpiiu::versionMessage`
/// (`tcpiiu.cpp:1393-1397`) passes `priority` as the dataType and
/// `CA_MINOR_PROTOCOL_REVISION` as the count. Pre-fix Rust left dataType
/// at 0 (priorityDefault), so a server could not see the client's
/// requested priority.
/// Build one CA identity frame — `CA_PROTO_CLIENT_NAME` (user name) or
/// `CA_PROTO_HOST_NAME` (host name) — carrying `value` as a NUL-padded
/// string payload.
///
/// Single source of the on-wire identity-frame shape, shared by both the
/// connect-time handshake ([`build_client_handshake`]) and the
/// runtime-rename broadcast (`CaClient::set_user_name` /
/// `set_host_name`). C `libca/tcpiiu.cpp::userNameSetRequest` /
/// `hostNameSetRequest` route through `comQueSend::insertRequestHeader`,
/// which emits the extended-size annex when the payload exceeds 16 bits;
/// `set_payload_size` + `to_bytes_extended` reproduce that. A separate
/// hand-rolled builder that truncated the postsize via `as u16` while
/// still writing the full payload is what desynchronised the circuit for
/// an oversized USER/host value — keeping every identity frame on this
/// one builder closes that family out.
pub(crate) fn build_identity_frame(cmd: u16, value: &str) -> Vec<u8> {
    let payload = pad_string(value);
    let mut hdr = CaHeader::new(cmd);
    hdr.set_payload_size(payload.len(), 0);
    let mut frame = hdr.to_bytes_extended();
    frame.extend_from_slice(&payload);
    frame
}

fn build_client_handshake(priority: u8, identity: &super::types::ClientIdentitySlot) -> Vec<u8> {
    let mut handshake = Vec::new();
    let mut version_hdr = CaHeader::new(CA_PROTO_VERSION);
    version_hdr.count = CA_MINOR_VERSION;
    version_hdr.data_type = priority as u16;
    handshake.extend_from_slice(&version_hdr.to_bytes());
    // Snapshot the shared identity once. `CaClient::set_user_name` /
    // `set_host_name` mutate this slot at runtime, so a circuit formed
    // after a rename handshakes with the new names; circuits already
    // established are notified separately by an out-of-band identity
    // frame.
    let (username, hostname) = {
        let id = identity.read();
        (id.user.clone(), id.host.clone())
    };
    handshake.extend_from_slice(&build_identity_frame(CA_PROTO_CLIENT_NAME, &username));
    handshake.extend_from_slice(&build_identity_frame(CA_PROTO_HOST_NAME, &hostname));
    handshake
}

#[allow(clippy::too_many_arguments)]
async fn connect_server(
    server_addr: SocketAddr,
    priority: u8,
    event_tx: mpsc::UnboundedSender<TransportEvent>,
    in_flight: super::types::InFlightOps,
    last_rx_at: super::types::ServerLastRxAt,
    identity: super::types::ClientIdentitySlot,
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

    // Build initial CA handshake.
    let handshake = build_client_handshake(priority, &identity);
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
        // cap the client-side TLS handshake. A misbehaving (or
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
            priority,
            event_tx.clone(),
            pending_frames.clone(),
        ));
        let read_task = epics_base_rs::runtime::task::spawn(read_loop(
            reader,
            server_addr,
            priority,
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
            priority,
            event_tx.clone(),
            pending_frames.clone(),
        ));
        let read_task = epics_base_rs::runtime::task::spawn(read_loop(
            reader,
            server_addr,
            priority,
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
            priority,
            event_tx.clone(),
            pending_frames.clone(),
        ));
        let read_task = epics_base_rs::runtime::task::spawn(read_loop(
            reader,
            server_addr,
            priority,
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
    priority: u8,
    event_tx: mpsc::UnboundedSender<TransportEvent>,
    pending_frames: std::sync::Arc<std::sync::atomic::AtomicUsize>,
) {
    // Send watchdog deadline. C `tcpSendWatchdog`
    // (`libca/tcpSendWatchdog.cpp:43-64`) fires after `connTMO`
    // (`EPICS_CA_CONN_TMO`, default 30 s) and calls
    // `iiu.sendTimeoutNotify` → `unresponsiveCircuitNotify`. The
    // critical detail is that C's send is a *blocking* `::send`
    // (`tcpiiu.cpp:232`, `flushToWire`): the watchdog runs as a
    // separate liveness probe alongside a send that is never
    // abandoned mid-frame — `flushToWire` either completes a whole
    // `comBuf` or fails. C therefore never leaves a truncated CA
    // frame on the wire and then keeps writing.
    //
    // Tokio's `timeout(send_timeout, write_all(&batch))` is NOT
    // equivalent: when the timeout fires the `write_all` future is
    // *cancelled*, possibly after a prefix of a CA frame has already
    // reached the socket. Continuing to write later batches on the
    // same stream would append after that truncated frame and
    // desynchronize the server's parser. This arm was changed to
    // emit `CircuitUnresponsive` and keep the socket — that is only
    // safe for the read-side echo watchdog, where no bytes were
    // corrupted, not for a cancelled write. So on write timeout we
    // close the circuit (`TcpClosed`), exactly as `origin/main` did:
    // `handle_disconnect` then drains the pending read/write waiters
    // (no silently-dropped frames) and the reconnect path rebuilds a
    // clean stream. This mirrors C's send-error path, where a
    // `flushToWire` failure makes `sendThreadFlush` return false →
    // the send thread breaks and `shutdown(sock, SHUT_WR)` tears the
    // circuit down (`tcpiiu.cpp:168-176`, `:1684-1690`).
    let send_timeout = echo_idle();
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
                // `pending_frames` is the local backpressure
                // counter that decides when `send_frame` should treat
                // a stalled circuit as disconnected. Pre-fix the
                // decrement used `load` + `store(prev - drained)`,
                // which silently overwrites any concurrent
                // `send_frame::fetch_add` landing between the two
                // operations — under sustained producer activity the
                // counter drifted below the real queued-frame count
                // and the SEND_BACKPRESSURE_FRAMES threshold stopped
                // firing reliably. `fetch_sub` is the atomic
                // equivalent and never loses a concurrent increment.
                // `saturating_sub` semantics: a `read_loop` frame
                // (echo / flow control) bypasses `send_frame`'s
                // increment, so the counter occasionally undershoots
                // `drained`; `fetch_sub` would wrap on underflow, so
                // pre-clamp with a CAS loop.
                let mut current = pending_frames.load(std::sync::atomic::Ordering::Relaxed);
                loop {
                    let next = current.saturating_sub(drained);
                    match pending_frames.compare_exchange_weak(
                        current,
                        next,
                        std::sync::atomic::Ordering::Relaxed,
                        std::sync::atomic::Ordering::Relaxed,
                    ) {
                        Ok(_) => break,
                        Err(observed) => current = observed,
                    }
                }
            }
            Ok(Err(_)) => {
                // True socket error (write_all returned Err) — circuit
                // is dead, signal close.
                let _ = event_tx.send(TransportEvent::TcpClosed {
                    server_addr,
                    priority,
                });
                return;
            }
            Err(_) => {
                // write-side timeout. `write_all` was
                // cancelled by `timeout`, so a prefix of a CA frame
                // may already be on the wire. The TCP stream is no
                // longer a clean message boundary — keeping it and
                // writing later batches would concatenate after a
                // truncated frame and desync the server parser.
                // Close the circuit: `handle_disconnect` (mod.rs)
                // drains the pending read/write waiters so callers
                // get a deterministic failure instead of hanging,
                // and the reconnect path rebuilds a clean stream.
                // The per-connection `pending_frames` backpressure
                // counter is discarded with the dropped
                // `ServerConnection`, so the stale count cannot
                // leak into a future circuit.
                let _ = event_tx.send(TransportEvent::TcpClosed {
                    server_addr,
                    priority,
                });
                return;
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn read_loop<R: AsyncRead + Unpin + Send + 'static>(
    mut reader: R,
    server_addr: SocketAddr,
    priority: u8,
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
    // libca Issue #190 (laptop-suspend stall): the OS may pause the
    // tokio reactor for many minutes during system suspend. On
    // resume, `Instant::now()` jumps forward by ~the suspend
    // duration. The Sleep fires immediately (deadline in the past)
    // and sends an echo — but the TCP socket may be half-open and
    // we'd otherwise wait the full 5 s echo timeout to find out.
    // Track wall-clock between loop iterations; if it skips more than
    // `SUSPEND_THRESHOLD` (3× idle_timeout — large enough to ignore
    // ordinary scheduling jitter, small enough to fire on a real
    // suspend of even a few minutes) we use the abbreviated echo
    // timeout so recovery completes in seconds instead of tens.
    const SUSPEND_PROBE_TIMEOUT: Duration = Duration::from_secs(1);
    let suspend_threshold = idle_timeout.saturating_mul(3).max(Duration::from_secs(60));
    // Anchor for suspend detection — refreshed at the top of every
    // loop iteration. Initialized at the loop entry; first iteration
    // overwrites it before the wall-clock skip is consulted.
    let mut last_loop_at;
    // libca `tcpRecvWatchdog::beaconAnomaly` flag. Set when the
    // beacon monitor classifies a beacon as a real restart signal
    // (`IdMismatch` / `PeriodCollapse`); suppresses subsequent
    // healthy-beacon watchdog refreshes so the deadline expires on
    // its own schedule. Cleared on any data arrival from the server.
    let mut beacon_anomaly = false;
    let mut unresponsive_notified = false;
    let mut server_minor_version: u16 = 0;
    let mut beacon_rx_open = true;
    // C `claim_ciu_reply` (`rsrv/camessage.c:1149-1175`) emits the
    // CA_PROTO_ACCESS_RIGHTS frame BEFORE the CA_PROTO_CREATE_CHAN
    // reply on the same TCP stream. The coordinator's
    // `AccessRightsChanged` handler at `mod.rs:2531` looks up the
    // channel by cid — but the channel doesn't exist in the
    // coordinator's map until `ChannelCreated` arrives second. So
    // the access bits get silently dropped, and the
    // `ChannelCreated` event hard-coded `AccessRights::from_u32(0x3)`
    // (full READ+WRITE) regardless of what the server actually
    // granted. Result: a Rust client against a read-only PV could
    // attempt writes that the server rejects later (ECA_NOWTACCESS),
    // instead of refusing them client-side from the access cache.
    //
    // Stash by cid; consumed when the matching CREATE_CHAN reply
    // arrives. If multiple ACCESS_RIGHTS frames arrive between
    // CREATE_CHAN cycles (rare but legal — server may emit
    // mid-stream on ACF reload), only the most recent is kept.
    let mut pending_access: std::collections::HashMap<u32, AccessRights> =
        std::collections::HashMap::new();
    // cids the server has acknowledged via CREATE_CHAN. An
    // ACCESS_RIGHTS frame for a known cid is a *post-create* update
    // (ACF reload, server-side rule change) and must fire the event.
    // An ACCESS_RIGHTS frame for an unknown cid is a *pre-create*
    // stash — the matching CREATE_CHAN reply consumes it and the
    // ChannelCreated event already carries the access, so no
    // AccessRightsChanged is needed in that path. Pre-fix Rust
    // emitted the event in both cases; combined with the stash
    // cap, a stray-ACCESS_RIGHTS-flood from a hostile server loaded
    // the unbounded event_tx mpsc one message per stray frame even
    // though the coordinator's downstream filter dropped them all.
    let mut known_cids: std::collections::HashSet<u32> = std::collections::HashSet::new();
    // rate-limit the cap-hit warning so a hostile flood does
    // not also flood the logs. One warn per circuit lifetime is
    // enough — the metric `ca_client_pending_access_evictions_total`
    // carries the running count for observability.
    let mut cap_warned = false;

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
        // Refresh the suspend-detection anchor at the top of each
        // loop iteration. When the sleep branch wakes, `wall_skip =
        // SystemTime::now() - last_loop_at` reveals whether the host
        // was suspended during the await — sleep duration on a live
        // system stays bounded by `idle_timeout`/`echo_timeout`, so a
        // skip far beyond that is a strong suspend signal.
        last_loop_at = std::time::SystemTime::now();
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
                // libca Issue #190: detect suspend wake. If wall-clock
                // skipped far more than expected for this sleep, the
                // tokio reactor was paused (laptop suspend / VM stop).
                // Shorten the echo probe so recovery is seconds, not
                // tens of seconds.
                let now_wall = std::time::SystemTime::now();
                let wall_skip = now_wall
                    .duration_since(last_loop_at)
                    .unwrap_or(Duration::ZERO);
                let suspend_wake = wall_skip >= suspend_threshold;
                if echo_pending {
                    if !unresponsive_notified {
                        let _ = event_tx
                            .send(TransportEvent::CircuitUnresponsive { server_addr, priority });
                        unresponsive_notified = true;
                        if send_echo(&write_tx, server_minor_version).is_err() {
                            let _ = event_tx.send(TransportEvent::TcpClosed { server_addr, priority });
                            return;
                        }
                        let probe = if suspend_wake {
                            SUSPEND_PROBE_TIMEOUT
                        } else {
                            echo_timeout
                        };
                        deadline = tokio::time::Instant::now() + probe;
                        sleep.as_mut().reset(deadline);
                        continue;
                    }
                    // Second echo timeout — truly dead.
                    let _ = event_tx.send(TransportEvent::TcpClosed { server_addr, priority });
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
                    let _ = event_tx.send(TransportEvent::TcpClosed { server_addr, priority });
                    return;
                }
                echo_pending = true;
                let probe = if suspend_wake {
                    tracing::info!(
                        server = %server_addr,
                        wall_skip_secs = wall_skip.as_secs(),
                        "suspend wake detected; probing with shortened echo timeout"
                    );
                    SUSPEND_PROBE_TIMEOUT
                } else {
                    echo_timeout
                };
                deadline = tokio::time::Instant::now() + probe;
                sleep.as_mut().reset(deadline);
                continue;
            }
            // Data from the server.
            read_result = reader.read(&mut buf) => {
                match read_result {
                    Ok(0) | Err(_) => {
                        let _ = event_tx.send(TransportEvent::TcpClosed { server_addr, priority });
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
        last_rx_at.insert((server_addr, priority), std::time::Instant::now());
        if unresponsive_notified {
            unresponsive_notified = false;
            let _ = event_tx.send(TransportEvent::CircuitResponsive {
                server_addr,
                priority,
            });
        }

        // Automatic CA flow control is intentionally disabled here. The
        // previous implementation counted TCP reads, which can overshoot badly
        // on fragmented links and stall remote C IOCs with EVENTS_OFF. A
        // correct implementation must count parsed monitor messages and resume
        // based on downstream consumption, not socket read timing.
        accumulated.extend_from_slice(&buf[..n]);

        // Guard against unbounded buffer growth from malformed servers.
        let accum_cap = max_accumulated();
        if accumulated.len() > accum_cap {
            eprintln!(
                "CA: {server_addr}: accumulated TCP buffer exceeded {accum_cap} bytes, closing"
            );
            let _ = event_tx.send(TransportEvent::TcpClosed {
                server_addr,
                priority,
            });
            return;
        }

        let mut offset = 0;
        while offset + CaHeader::SIZE <= accumulated.len() {
            let frame = &accumulated[offset..];
            // C `tcpiiu.cpp::processIncoming` distinguishes a *partial*
            // extended header (await more bytes) from a *definitively
            // malformed* one (close the connection). `from_bytes_extended`
            // returns `Err` for both, so a blanket `break` (await more)
            // would spin: a malformed header is re-parsed on every read,
            // never closing until the accumulation cap is hit. Detect the
            // only legitimate "await more" case — an extended-form header
            // (`postsize == 0xFFFF`) with fewer than its 24 bytes present
            // — and treat every other parse error as a hard close.
            let is_partial_extended_header =
                frame.len() >= 4 && frame[2] == 0xFF && frame[3] == 0xFF && frame.len() < 24;
            let (hdr, hdr_size) = match CaHeader::from_bytes_extended(frame) {
                Ok(v) => v,
                Err(_) if is_partial_extended_header => {
                    // Genuine TCP segment boundary inside an extended
                    // header — wait for the remaining bytes.
                    break;
                }
                Err(e) => {
                    // C `libca/tcpiiu.cpp:1269-1284` logs ONCE
                    // and skips an oversized message (`m_postsize >
                    // curDataMax` with realloc failure) via
                    // `recvQue.removeBytes` — circuit kept alive.
                    // Pre-fix Rust always closed. Try to recover the
                    // same way: re-read the announced postsize and
                    // skip header + payload if the bytes are present.
                    let base_post = u16::from_be_bytes([frame[2], frame[3]]) as usize;
                    let skip = if base_post == 0xFFFF && frame.len() >= 24 {
                        let ext_post =
                            u32::from_be_bytes([frame[16], frame[17], frame[18], frame[19]])
                                as usize;
                        // Sanity cap to stop a corrupted ext_post from
                        // forcing us to wait for gigabytes of "data";
                        // if the announced size dwarfs the buffer cap
                        // it's safer to close.
                        if ext_post > max_payload_size() * 2 {
                            None
                        } else {
                            Some(24 + ext_post)
                        }
                    } else if base_post == 0xFFFF {
                        // Need annex bytes before we can recover.
                        break;
                    } else {
                        Some(16 + base_post)
                    };
                    if let Some(skip_n) = skip {
                        if accumulated.len() - offset >= skip_n {
                            tracing::warn!(
                                server = %server_addr,
                                err = %e,
                                skip = skip_n,
                                "CA: oversized / unparseable TCP frame; skipping (libca tcpiiu:1269-1284 parity)"
                            );
                            metrics::counter!("ca_client_oversized_frame_skips_total").increment(1);
                            offset += skip_n;
                            continue;
                        } else {
                            // Wait for the rest of the bytes before
                            // skipping.
                            break;
                        }
                    } else {
                        eprintln!("CA: {server_addr}: malformed TCP header ({e}), closing");
                        let _ = event_tx.send(TransportEvent::TcpClosed {
                            server_addr,
                            priority,
                        });
                        return;
                    }
                }
            };
            let actual_post = hdr.actual_postsize();
            // C `tcpiiu.cpp::processIncoming` (line 1198) closes the
            // connection if `m_postsize & 0x7 != 0`. The wire spec
            // requires every payload to be 8-byte aligned; an
            // unaligned postsize is either a malformed peer or an
            // attempt to push our parser into reading the next
            // message's header as the tail of this one. Silently
            // rounding via `align8` (the prior behavior) lets a
            // malicious server desync our framer. Match C: drop the
            // connection so the reconnect loop can rebuild from a
            // clean state.
            if actual_post & 0x7 != 0 {
                eprintln!(
                    "CA: {server_addr}: misaligned payload (postsize={actual_post}), closing"
                );
                let _ = event_tx.send(TransportEvent::TcpClosed {
                    server_addr,
                    priority,
                });
                return;
            }
            let msg_len = hdr_size + actual_post;

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
                        priority,
                        minor_version: hdr.count,
                    });
                }
                CA_PROTO_ACCESS_RIGHTS => {
                    let access = AccessRights::from_u32(hdr.available);
                    // Stash for the next CREATE_CHAN reply on this
                    // cid (C orders ACCESS_RIGHTS first; the
                    // coordinator's update-by-cid is a no-op
                    // pre-channel).
                    //
                    // bound the stash size. C `libca/cac.cpp:
                    // 1121-1136` `accessRightsRespAction` looks up by
                    // m_cid and silently returns if not found — never
                    // accumulates state. Pre-fix Rust grew the map on
                    // every ACCESS_RIGHTS frame, so a misbehaving /
                    // hostile server emitting ACCESS_RIGHTS for cids
                    // that never get named in CREATE_CHAN leaked one
                    // entry per frame for the circuit's lifetime.
                    // 1024 is well past the per-client channel cap
                    // any realistic deployment hits; well below the
                    // memory pressure threshold.
                    // post-create ACCESS_RIGHTS goes straight
                    // to the coordinator as an update event; the
                    // pre-create path stashes for the CREATE_CHAN
                    // consumer (which folds it into ChannelCreated).
                    if known_cids.contains(&hdr.cid) {
                        let _ = event_tx.send(TransportEvent::AccessRightsChanged {
                            cid: hdr.cid,
                            access,
                        });
                    } else {
                        // bound the stash size. C
                        // `libca/cac.cpp:1121-1136`
                        // `accessRightsRespAction` looks up by m_cid
                        // and silently returns if not found — never
                        // accumulates. 1024 is well past the per-
                        // client channel cap any realistic deployment
                        // hits; well below memory pressure.
                        const PENDING_ACCESS_CAP: usize = 1024;
                        if pending_access.len() >= PENDING_ACCESS_CAP {
                            if let Some(&victim) = pending_access.keys().next() {
                                pending_access.remove(&victim);
                                metrics::counter!("ca_client_pending_access_evictions_total")
                                    .increment(1);
                                // log the cap-hit ONCE per
                                // circuit so operators can correlate
                                // with a misbehaving server. C never
                                // accumulates so this condition can't
                                // exist in C; we mirror C's silent-
                                // on-unknown-cid behaviour at steady
                                // state but surface the new failure
                                // mode (cap exceeded) at warn level.
                                if !cap_warned {
                                    cap_warned = true;
                                    tracing::warn!(
                                        target: "epics_ca_rs::client::transport",
                                        cap = PENDING_ACCESS_CAP,
                                        "pending_access cap reached — server is emitting \
                                         ACCESS_RIGHTS for cids no CREATE_CHAN names; oldest \
                                         entry evicted. Further evictions are silent; see \
                                         metric ca_client_pending_access_evictions_total"
                                    );
                                }
                            }
                        }
                        pending_access.insert(hdr.cid, access);
                    }
                }
                CA_PROTO_CREATE_CHAN => {
                    // Consume the stashed ACCESS_RIGHTS for this cid
                    // if any (C `claim_ciu_reply` always emits one
                    // first; falls back to NoAccess if missing —
                    // defensive default since we can't assume
                    // RW on an open channel).
                    let access = pending_access
                        .remove(&hdr.cid)
                        .unwrap_or(AccessRights::from_u32(0));
                    // now that the server has named this cid,
                    // subsequent ACCESS_RIGHTS frames for it are
                    // legitimate post-create updates that must fire
                    // AccessRightsChanged.
                    known_cids.insert(hdr.cid);
                    let _ = event_tx.send(TransportEvent::ChannelCreated {
                        cid: hdr.cid,
                        sid: hdr.available,
                        data_type: hdr.data_type,
                        element_count: hdr.actual_count(),
                        access,
                        server_addr,
                        priority,
                    });
                }
                CA_PROTO_READ_NOTIFY => {
                    // Direct dispatch to the in-flight read registry
                    // (Option C Phase A) — bypasses the coordinator's
                    // `tokio::select!` loop. Plain scalar reads are
                    // decoded here so the hot path does not allocate
                    // one payload Vec per response.
                    let ioid = hdr.available;
                    if hdr.cid == ECA_NORMAL {
                        let data = &accumulated[data_start..data_start + actual_post];
                        dispatch_read_reply_with(&in_flight, ioid, |mode| {
                            make_read_reply(mode, hdr.data_type, hdr.actual_count(), data)
                        });
                    } else {
                        dispatch_read_error(
                            &in_flight,
                            ioid,
                            epics_base_rs::error::CaError::Protocol(format!(
                                "server returned ECA error {:#06x}",
                                hdr.cid
                            )),
                        );
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
                    // libca `cac::eventAddRespAction` (`cac.cpp:960`)
                    // gates the data delivery on `hdr.m_cid ==
                    // ECA_NORMAL`. The CA server uses non-NORMAL m_cid
                    // values on monitor frames to deliver out-of-band
                    // status to the subscriber — specifically
                    // `rsrv/camessage.c::no_read_access_event` emits
                    // ECA_NORDACCESS with a zeroed payload of full
                    // DBR size when read access for an active
                    // subscription is denied (e.g. after an ACF
                    // reload that revokes the client's identity).
                    // Without the gate, Rust would parse the zeroed
                    // payload as legitimate data and surface
                    // `value = 0` to the subscriber — silent
                    // "successful read of zero" instead of an access
                    // denial.
                    //
                    // The Rust SERVER tears down subscriptions on
                    // NoAccess, so Rust ↔ Rust never hits
                    // this path. But Rust client ↔ C IOC does — C IOC
                    // delivers the no-read-access frame instead of
                    // tearing down. Gate matches libca.
                    //
                    // C `libca/cac.cpp::eventRespAction()`
                    // returns immediately when `!hdr.m_postsize`,
                    // BEFORE the status/payload handling. Rsrv's
                    // `event_cancel_reply` intentionally sends a
                    // zero-payload `CA_PROTO_EVENT_ADD` confirmation;
                    // treating it as monitor data or as a status
                    // error surfaces the cancel ack as a bogus
                    // monitor event in the rare race where the
                    // subscription record is still present. The
                    // `if/else if/else` chain below skips the entire
                    // monitor delivery path when this is set — the
                    // outer `offset += msg_len` still advances.
                    if actual_post == 0 {
                        // zero-payload EVENT_ADD = cancel ack, drop silently
                    } else if hdr.cid != ECA_NORMAL {
                        // libca `cac::eventAddRespAction`
                        // (`cac.cpp:973-977`): when the monitor frame
                        // carries a non-NORMAL status, drop the
                        // (zeroed) payload but route the status
                        // through the per-subscription exception
                        // callback. Pre-fix Rust just warn+dropped,
                        // so e.g. an ECA_NORDACCESS from a C IOC's
                        // `no_read_access_event` (sent when an ACF
                        // reload revoked read access on an active
                        // subscription) was invisible to the
                        // subscriber. The bogus zeroed payload is
                        // still discarded — only the status is
                        // delivered — because libca only invokes
                        // `pmiu->exception(status)`, never the
                        // completion callback, on this path.
                        tracing::warn!(
                            server = %server_addr,
                            subid = hdr.available,
                            status = hdr.cid,
                            "MONITOR status error (libca: routes through subscription exception callback)"
                        );
                        metrics::counter!("ca_client_monitor_status_drops_total").increment(1);
                        let _ = event_tx.send(TransportEvent::MonitorStatusError {
                            subid: hdr.available,
                            eca_status: hdr.cid,
                        });
                    } else {
                        let data = accumulated[data_start..data_start + actual_post].to_vec();
                        let _ = event_tx.send(TransportEvent::MonitorData {
                            subid: hdr.available,
                            data_type: hdr.data_type,
                            count: hdr.actual_count(),
                            data,
                        });
                    }
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
                    // CA_PROTO_ERROR wire layout per C `vsend_err`
                    // (`rsrv/camessage.c:139-224`):
                    //   resp.m_cid       = channel cid (or
                    //                      0xFFFFFFFF for non-channel-
                    //                      scoped commands like SEARCH
                    //                      or unknown-cmd reject)
                    //   resp.m_available = ECA status code (caerr.h)
                    //   payload          = original 16-byte header copy
                    //                      + NUL-terminated diag msg
                    //
                    // libca `cac::exceptionRespAction`
                    // (`modules/ca/src/client/cac.cpp:1118`) passes
                    // `hdr.m_available` as the status to the per-cmd
                    // exception stub — `m_available` is authoritative.
                    //
                    // Commit 21240ad fixed the same field-swap on the
                    // Rust SERVER side; this round closes it on the
                    // Rust CLIENT side. Pre-fix Rust read `hdr.cid` as
                    // the ECA status, so a CA_PROTO_ERROR from a C IOC
                    // surfaced the channel cid as the user-facing
                    // `CaException.status` — the actual ECA code (and
                    // therefore the entire exception-callback contract)
                    // was wrong. Symptom: clients can't distinguish
                    // ECA_BADTYPE from ECA_NORDACCESS etc.
                    let eca_status = hdr.available;
                    let orig_cmd = if actual_post >= 16 {
                        let orig_hdr_bytes = &accumulated[data_start..data_start + 16];
                        Some(u16::from_be_bytes([orig_hdr_bytes[0], orig_hdr_bytes[1]]))
                    } else {
                        None
                    };
                    // C `cac::exceptionRespAction` (`cac.cpp:1097-1107`):
                    // when the echoed request used the extended layout
                    // (`m_postsize == 0xffff`), the 8-byte annex carrying
                    // the full 32-bit postsize and count sits between the
                    // 16-byte header echo and the diagnostic string.
                    // Pre-fix Rust unconditionally started the diag at
                    // `data_start + 16`, so an extended READ/WRITE error
                    // surfaced the annex bytes as the leading 8 bytes of
                    // `msg` (mis-framed for the caller) and any actual
                    // diag string was truncated by 8 bytes.
                    let echo_hdr_size = if actual_post >= 16
                        && u16::from_be_bytes([
                            accumulated[data_start + 2],
                            accumulated[data_start + 3],
                        ]) == 0xFFFF
                        && actual_post >= 24
                    {
                        24
                    } else {
                        16
                    };
                    let msg = if actual_post > echo_hdr_size {
                        let msg_bytes =
                            &accumulated[data_start + echo_hdr_size..data_start + actual_post];
                        let end = msg_bytes
                            .iter()
                            .position(|&b| b == 0)
                            .unwrap_or(msg_bytes.len());
                        String::from_utf8_lossy(&msg_bytes[..end]).to_string()
                    } else {
                        String::new()
                    };
                    // route to the in-flight operation registry
                    // matching the echoed request command. libca
                    // `cac::exceptionRespAction` (`cac.cpp:1081-1119`)
                    // dispatches by original command through
                    // `tcpExcepJumpTableCAC`; readNotifyExcep /
                    // writeNotifyExcep use `hdr.m_available` to
                    // complete and uninstall the pending IO callback
                    // so the user-facing `get()` / `put()` future
                    // surfaces the per-op error instead of timing
                    // out. Pre-fix Rust only fired the global
                    // exception hook here, leaving the per-op
                    // futures pending until their own timeout.
                    // Extract `m_available` from the echoed 16-byte
                    // header (offset 12) — the extended annex
                    // (postsize/count fields) is appended AFTER the
                    // 16-byte header, so `m_available` is always at
                    // the same position regardless of extended form.
                    let echo_available = if actual_post >= 16 {
                        Some(u32::from_be_bytes([
                            accumulated[data_start + 12],
                            accumulated[data_start + 13],
                            accumulated[data_start + 14],
                            accumulated[data_start + 15],
                        ]))
                    } else {
                        None
                    };
                    if let (Some(cmd), Some(ioid)) = (orig_cmd, echo_available) {
                        match cmd {
                            CA_PROTO_READ_NOTIFY => {
                                dispatch_read_error(
                                    &in_flight,
                                    ioid,
                                    epics_base_rs::error::CaError::ServerError(eca_status),
                                );
                            }
                            CA_PROTO_WRITE_NOTIFY => {
                                if let Some((_, (_, reply_tx))) = in_flight.writes.remove(&ioid) {
                                    let _ = reply_tx.send(Err(
                                        epics_base_rs::error::CaError::ServerError(eca_status),
                                    ));
                                }
                            }
                            // EVENT_ADD errors travel through
                            // MonitorStatusError (Bug 5 path); EVENT_CANCEL
                            // confirmations don't have a per-op waiter.
                            _ => {}
                        }
                    }
                    // C ref: modules/ca/src/client/udpiiu.cpp:exceptionRespAction —
                    // commit a352865 routes the error prefix through ERL_ERROR
                    // (ANSI-colored "Error:" on TTYs). The Rust equivalent is
                    // tracing::error! which honors the configured subscriber's
                    // formatting (color, prefix, structured fields).
                    tracing::error!(
                        server = %server_addr,
                        eca = eca_status,
                        cmd = ?orig_cmd,
                        msg = %msg,
                        "CA server error",
                    );
                    let _ = event_tx.send(TransportEvent::ServerError {
                        eca_status,
                        original_request: orig_cmd,
                        message: msg,
                        server_addr,
                    });
                }
                CA_PROTO_SERVER_DISCONN => {
                    // server retired this cid — drop it from
                    // the post-create set so a same-cid CREATE_CHAN
                    // reuse later in the circuit starts fresh.
                    known_cids.remove(&hdr.cid);
                    pending_access.remove(&hdr.cid);
                    let _ = event_tx.send(TransportEvent::ServerDisconnect {
                        cid: hdr.cid,
                        server_addr,
                    });
                }
                // opcodes that C `libca/cac.cpp:60-89`
                // dispatches through its TCP jump table but Rust
                // didn't have a per-opcode arm for. Rust once made
                // unknown opcodes lethal (close circuit), so
                // benign frames from a gateway / name-server
                // / legacy IOC ended up tearing the Rust circuit
                // down on every occurrence. Accept them here as
                // no-ops:
                //   * CA_PROTO_SEARCH (6) — used when a CA
                //     server doubles as a name server
                //     (EPICS_CA_NAME_SERVERS); libca routes via
                //     `tcpiiu::searchRespNotify` and our TCP
                //     search path already has a separate
                //     nameserver pipeline.
                //   * CA_PROTO_READ (3) — deprecated synchronous
                //     read response; libca handles via
                //     `cac::readRespAction`. Rust never sends
                //     CA_PROTO_READ (only READ_NOTIFY), so any
                //     reply on this opcode is informational.
                //   * CA_PROTO_CLEAR_CHANNEL (12) — `cac.cpp:
                //     1000-1003` `clearChannelRespAction` is
                //     currently a documented no-op in C.
                CA_PROTO_SEARCH | CA_PROTO_READ | CA_PROTO_CLEAR_CHANNEL => {
                    tracing::trace!(
                        server = %server_addr,
                        cmd = hdr.cmmd,
                        "TCP no-op opcode received (libca-recognised, Rust ignores)"
                    );
                }
                unknown => {
                    // C `libca/cac.cpp::executeResponse()`
                    // dispatches unknown opcodes to
                    // `badTCPRespAction()`, which logs and returns
                    // false; `tcpiiu.cpp` treats
                    // `processIncoming() == false` as a protocol
                    // failure and calls `initiateAbortShutdown()`.
                    // Pre-fix Rust skipped unknown opcodes
                    // silently — a broken or hostile server could
                    // inject response frames that libca uses to
                    // tear down the circuit while Rust quietly
                    // advanced past them. Emit TcpClosed so the
                    // coordinator drops the circuit; the
                    // surrounding reconnect path will rebuild.
                    tracing::warn!(
                        server = %server_addr,
                        cmd = unknown,
                        "unknown TCP response opcode; closing circuit (C badTCPRespAction parity)"
                    );
                    metrics::counter!("ca_client_bad_tcp_response_total").increment(1);
                    let _ = event_tx.send(TransportEvent::TcpClosed {
                        server_addr,
                        priority,
                    });
                    return;
                }
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
            0,
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

#[cfg(test)]
mod server_connection_drop_tests {
    //! Verifies the per-circuit `ServerConnection::drop` aborts both
    //! its read and write tasks. Without this, every `connections`
    //! HashMap drop path (send-buffer-stall removal, transport
    //! manager exit, `CaClient::drop`) would detach the JoinHandles
    //! and leave the spawned per-server tasks running until process
    //! exit. The companion `CaClient::Drop` only aborts top-level
    //! tasks (coordinator / search / transport / beacon); this
    //! per-connection Drop is what makes the cascade complete.
    use super::*;
    use std::time::Duration;
    use tokio::sync::mpsc;

    #[tokio::test]
    async fn drop_aborts_read_and_write_tasks() {
        // Long-running dummy tasks that never complete on their own.
        let read_task = tokio::spawn(async {
            loop {
                tokio::time::sleep(Duration::from_secs(60)).await;
            }
        });
        let write_task = tokio::spawn(async {
            loop {
                tokio::time::sleep(Duration::from_secs(60)).await;
            }
        });
        // AbortHandle sticks around after the JoinHandle is moved
        // into ServerConnection — lets us observe the post-drop
        // task state.
        let read_abort = read_task.abort_handle();
        let write_abort = write_task.abort_handle();

        let (write_tx, _write_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let (beacon_arrival_tx, _ba_rx) = mpsc::unbounded_channel::<bool>();

        let conn = ServerConnection {
            write_tx,
            pending_frames: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            beacon_arrival_tx,
            _read_task: read_task,
            _write_task: write_task,
        };

        // Pre-drop: tasks are still running.
        assert!(!read_abort.is_finished());
        assert!(!write_abort.is_finished());

        drop(conn);

        // tokio's abort schedules cancellation; let the runtime
        // drain it.
        let drain_started = tokio::time::Instant::now();
        for _ in 0..50 {
            if read_abort.is_finished() && write_abort.is_finished() {
                break;
            }
            tokio::task::yield_now().await;
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        let drain_elapsed = drain_started.elapsed();

        assert!(
            read_abort.is_finished(),
            "ServerConnection::drop must abort _read_task"
        );
        assert!(
            write_abort.is_finished(),
            "ServerConnection::drop must abort _write_task"
        );

        // Reproducer guard for epics-base issue #477 (30s hang after
        // both ends are destroyed): if Drop ever stops aborting the
        // pumps, the test would loop the full 50 × 2 ms = 100 ms
        // budget then fail above. Tighten the budget here so a
        // regression toward "let echo timeout drain" (which would
        // approach the upstream 30 s symptom) shows up immediately.
        assert!(
            drain_elapsed < Duration::from_millis(500),
            "abort cascade took {drain_elapsed:?} — far over the \
             tens-of-milliseconds budget (#477 reproducer)"
        );
    }
}

#[cfg(test)]
mod framing_cap_tests {
    //! BUG 2: the accumulation cap must be >= the largest legal frame,
    //! otherwise a valid large waveform (under `max_payload_size()`)
    //! closes the connection permanently.
    use super::max_accumulated;
    use crate::protocol::max_payload_size;

    #[test]
    fn accumulation_cap_admits_largest_legal_frame() {
        // A legal frame is at most extended-header (24 bytes) +
        // `max_payload_size()`. The cap must strictly exceed it so the
        // full frame can sit in `accumulated` before being drained.
        let largest_legal_frame = max_payload_size() + 24;
        assert!(
            max_accumulated() >= largest_legal_frame,
            "accumulation cap {} is smaller than the largest legal frame {} \
             — a valid large waveform would be rejected (BUG 2)",
            max_accumulated(),
            largest_legal_frame
        );
    }

    #[test]
    fn accumulation_cap_admits_two_megabyte_waveform() {
        // The concrete BUG 2 repro: a 2 MB array payload is legal under
        // the 16 MB default and must NOT trip the cap.
        let two_mb_frame = 2 * 1024 * 1024 + 24;
        assert!(
            max_accumulated() >= two_mb_frame,
            "2 MB waveform frame ({two_mb_frame} bytes) exceeds the \
             accumulation cap ({}) — permanent failure for arrays > 1 MB",
            max_accumulated()
        );
    }
}

#[cfg(test)]
mod malformed_header_close_tests {
    //! BUG 3: the client read loop must distinguish a *partial*
    //! extended header (await more bytes) from a *definitively
    //! malformed* one (close the connection). A blanket "await more"
    //! spins forever re-parsing the same bad bytes until the
    //! accumulation cap is hit.
    use super::*;
    use dashmap::DashMap;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::io::AsyncWriteExt;

    fn loop_inputs() -> (
        SocketAddr,
        mpsc::UnboundedReceiver<TransportEvent>,
        mpsc::UnboundedSender<TransportEvent>,
        mpsc::UnboundedSender<Vec<u8>>,
        mpsc::UnboundedReceiver<bool>,
        super::super::types::InFlightOps,
        super::super::types::ServerLastRxAt,
    ) {
        let server_addr: SocketAddr = "127.0.0.1:5064".parse().unwrap();
        let (event_tx, event_rx) = mpsc::unbounded_channel::<TransportEvent>();
        let (write_tx, _write_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let (_ba_tx, ba_rx) = mpsc::unbounded_channel::<bool>();
        let in_flight = super::super::types::InFlightOps::new();
        let last_rx_at: super::super::types::ServerLastRxAt = Arc::new(DashMap::new());
        (
            server_addr,
            event_rx,
            event_tx,
            write_tx,
            ba_rx,
            in_flight,
            last_rx_at,
        )
    }

    /// A definitively malformed extended header (postsize=0xFFFF marking
    /// extended form, extended postsize declared far beyond
    /// `max_payload_size()`) must CLOSE the connection: `read_loop`
    /// emits `TcpClosed` and returns. Pre-fix it `break`d to await more
    /// bytes and re-parsed the same error on every read.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn malformed_extended_header_closes_connection() {
        let (server_addr, mut event_rx, event_tx, write_tx, ba_rx, in_flight, last_rx_at) =
            loop_inputs();
        let (client_io, server_io) = tokio::io::duplex(256);

        let loop_handle = tokio::spawn(read_loop(
            server_io,
            server_addr,
            0,
            event_tx,
            write_tx,
            ba_rx,
            in_flight,
            last_rx_at,
        ));

        // 24-byte extended header: postsize=0xFFFF (extended marker),
        // extended postsize set above the sanity cap (2x
        // max_payload_size()) so the skip-and-continue recovery
        // can't apply and the loop still closes the circuit.
        // Values <= 2x max_payload_size() are now treated as
        // recoverable (skip + continue) per libca tcpiiu:1269-1284.
        let mut hdr = CaHeader::new(CA_PROTO_EVENT_ADD);
        hdr.postsize = 0xFFFF;
        let mut frame = hdr.to_bytes().to_vec();
        let bad_post = (crate::protocol::max_payload_size() * 3) as u32;
        frame.extend_from_slice(&bad_post.to_be_bytes()); // extended postsize
        frame.extend_from_slice(&0u32.to_be_bytes()); // extended count
        assert_eq!(frame.len(), 24);

        let mut client = client_io;
        client.write_all(&frame).await.expect("write bad header");
        client.flush().await.expect("flush");

        // The read loop must close — emit TcpClosed and return — WITHOUT
        // waiting for more bytes. We keep the write half open so this
        // only passes if the loop closes on its own.
        let closed = tokio::time::timeout(Duration::from_secs(2), event_rx.recv())
            .await
            .expect("read_loop must close on a malformed header, not spin");
        assert!(
            matches!(closed, Some(TransportEvent::TcpClosed { .. })),
            "malformed extended header must emit TcpClosed"
        );
        let _ = tokio::time::timeout(Duration::from_secs(2), loop_handle).await;
        drop(client);
    }

    /// Control: a *partial* extended header (only 20 of 24 bytes) must
    /// NOT close — `read_loop` waits for the remaining bytes. Closing
    /// here would be a false-positive disconnect on a benign TCP
    /// segment boundary.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn partial_extended_header_waits_not_closes() {
        let (server_addr, mut event_rx, event_tx, write_tx, ba_rx, in_flight, last_rx_at) =
            loop_inputs();
        let (client_io, server_io) = tokio::io::duplex(256);

        let loop_handle = tokio::spawn(read_loop(
            server_io,
            server_addr,
            0,
            event_tx,
            write_tx,
            ba_rx,
            in_flight,
            last_rx_at,
        ));

        // 20 bytes: 16-byte base header with postsize=0xFFFF + only 4 of
        // the 8 extended bytes.
        let mut hdr = CaHeader::new(CA_PROTO_EVENT_ADD);
        hdr.postsize = 0xFFFF;
        let mut frame = hdr.to_bytes().to_vec();
        frame.extend_from_slice(&[0u8, 0, 0, 0]);
        assert_eq!(frame.len(), 20);

        let mut client = client_io;
        client.write_all(&frame).await.expect("write partial");
        client.flush().await.expect("flush");

        // No TcpClosed within 300ms — the loop is blocked awaiting bytes.
        let early = tokio::time::timeout(Duration::from_millis(300), event_rx.recv()).await;
        assert!(
            early.is_err(),
            "partial extended header must NOT close — read_loop waits \
             for the rest of the header"
        );

        // Clean EOF resolves the loop.
        drop(client);
        let _ = tokio::time::timeout(Duration::from_secs(2), loop_handle).await;
    }
}

#[cfg(test)]
mod write_loop_timeout_tests {
    //! a write-side timeout in `write_loop` must close the
    //! circuit (`TcpClosed`) rather than emit `CircuitUnresponsive`
    //! and keep writing on the same TCP stream. `tokio`'s
    //! `timeout(.., write_all(&batch))` cancels the `write_all`
    //! future on expiry, possibly after a prefix of a CA frame has
    //! already reached the socket; reusing that stream would
    //! concatenate later batches after a truncated frame and desync
    //! the server parser. Closing forces the reconnect path to
    //! rebuild a clean stream and lets `handle_disconnect` drain
    //! the pending waiters with a deterministic failure.
    use super::*;
    use std::pin::Pin;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::{Context, Poll};
    use std::time::Duration;
    use tokio::io::AsyncWrite;

    /// Mock writer that accepts a partial first write (simulating a
    /// CA-frame prefix landing on the socket) and then stalls
    /// forever — every later `poll_write` returns `Pending`. This is
    /// exactly the condition `write_all` cannot complete, so the
    /// `timeout` in `write_loop` fires.
    struct PartialThenStallWriter {
        first_write: Arc<AtomicUsize>,
    }

    impl AsyncWrite for PartialThenStallWriter {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            // First poll_write accepts a 4-byte prefix of the frame;
            // all subsequent ones stall (Pending) forever.
            if self.first_write.swap(1, Ordering::SeqCst) == 0 {
                let n = buf.len().min(4);
                Poll::Ready(Ok(n))
            } else {
                Poll::Pending
            }
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Pending
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    fn test_addr() -> SocketAddr {
        "127.0.0.1:5064".parse().unwrap()
    }

    /// Regression: a stalled write that has already accepted a
    /// partial frame must make `write_loop` emit `TcpClosed` and
    /// exit — NOT `CircuitUnresponsive` while keeping the socket.
    ///
    /// Pre-fix the timeout arm sent `CircuitUnresponsive`, cleared
    /// `batch`, and looped to drain the next frame on the same
    /// (now desynchronized) stream, silently dropping the timed-out
    /// frame and leaving the `pending_frames` counter inflated.
    #[tokio::test(start_paused = true)]
    async fn mr_r17_write_timeout_closes_circuit() {
        let server_addr = test_addr();
        let (event_tx, mut event_rx) = mpsc::unbounded_channel::<TransportEvent>();
        let (write_tx, write_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let pending_frames = Arc::new(AtomicUsize::new(0));
        let writer = PartialThenStallWriter {
            first_write: Arc::new(AtomicUsize::new(0)),
        };

        let task = tokio::spawn(write_loop(
            writer,
            write_rx,
            server_addr,
            0,
            event_tx,
            pending_frames.clone(),
        ));

        // Queue a CA frame. `send_frame` would have incremented
        // pending_frames; mirror that so we can assert the counter
        // is not stranded after the timeout.
        pending_frames.fetch_add(1, Ordering::SeqCst);
        write_tx
            .send(vec![0xAAu8; 32])
            .expect("frame enqueue must succeed");

        // The writer accepts 4 bytes then stalls; `echo_idle()`
        // (default CONN_TMO 30 s) elapses on the paused clock and
        // the timeout arm fires.
        let evt = tokio::time::timeout(Duration::from_secs(60), event_rx.recv())
            .await
            .expect("write_loop must emit an event before 60 s")
            .expect("event channel must not be closed");

        match evt {
            TransportEvent::TcpClosed { server_addr: a, .. } => assert_eq!(a, server_addr),
            TransportEvent::CircuitUnresponsive { .. } => panic!(
                "write timeout must CLOSE the circuit (TcpClosed): the \
                 cancelled write_all may have left a partial CA frame on \
                 the wire — keeping the socket desyncs the server parser"
            ),
            _ => panic!("write timeout must emit TcpClosed, got a different event"),
        }

        // The write loop must have returned (not looped to drain the
        // next frame on the dead stream).
        let joined = tokio::time::timeout(Duration::from_secs(2), task).await;
        assert!(
            joined.is_ok(),
            "write_loop must exit after a write-timeout close, not \
             continue writing on the desynchronized circuit"
        );
    }
}

#[cfg(test)]
mod priority_circuit_tests {
    //! priority is part of the virtual-circuit identity. Two
    //! channels to the same IOC at different priorities open independent
    //! TCP circuits (libca `caServerID = (addr, priority)`), the VERSION
    //! message carries the priority in its `m_dataType` field, and
    //! tearing one priority circuit down leaves the other connected.
    use super::*;
    use crate::client::types::{InFlightOps, ServerLastRxAt};
    use crate::protocol::{CA_PROTO_VERSION, CaHeader};
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    /// Identity slot resolved from this process's env — the same source
    /// the production `CaClient` uses, so handshake byte lengths in these
    /// tests match what the manager emits.
    fn test_identity() -> crate::client::types::ClientIdentitySlot {
        Arc::new(parking_lot::RwLock::new(
            crate::client::types::ClientIdentity::from_env(),
        ))
    }

    /// Test 2 (deterministic, no sockets): the VERSION frame the client
    /// emits puts the requested priority in `m_dataType` and the minor
    /// version in `m_count` — exactly libca's `versionMessage` layout.
    #[test]
    fn version_message_carries_priority_in_data_type() {
        let identity = test_identity();
        for pri in [0u8, 1, 7, 99] {
            let hs = build_client_handshake(pri, &identity);
            let hdr = CaHeader::from_bytes(&hs[..16]).expect("parse VERSION header");
            assert_eq!(
                hdr.cmmd, CA_PROTO_VERSION,
                "first frame is CA_PROTO_VERSION"
            );
            assert_eq!(
                hdr.data_type, pri as u16,
                "VERSION m_dataType must equal the requested priority"
            );
            assert_eq!(
                hdr.count, CA_MINOR_VERSION,
                "VERSION m_count must still carry the minor protocol version"
            );
        }
    }

    /// `build_identity_frame` must reproduce libca's extended-size annex:
    /// a sub-0xFFFF payload stays in the 16-byte header with `postsize`
    /// set directly; a payload at/over 0xFFFF pegs `postsize` to 0xFFFF
    /// and appends the real size in the 8-byte annex. A builder writing
    /// `len as u16` would truncate the size and desync the circuit — this
    /// guards both the handshake and the runtime-rename broadcast that
    /// share this builder.
    #[test]
    fn identity_frame_uses_extended_annex_for_oversized_payload() {
        let small = build_identity_frame(crate::protocol::CA_PROTO_CLIENT_NAME, "operator");
        let (hdr, consumed) = CaHeader::from_bytes_extended(&small).expect("parse small frame");
        assert_eq!(hdr.cmmd, crate::protocol::CA_PROTO_CLIENT_NAME);
        assert_eq!(consumed, 16, "small payload stays in the 16-byte header");
        assert!(hdr.extended_postsize.is_none());
        assert_eq!(hdr.postsize as usize, small.len() - consumed);

        let big_value = "h".repeat(0x1_0000); // 65536 > 0xFFFF
        let big = build_identity_frame(crate::protocol::CA_PROTO_HOST_NAME, &big_value);
        let (hdr, consumed) = CaHeader::from_bytes_extended(&big).expect("parse big frame");
        assert_eq!(hdr.cmmd, crate::protocol::CA_PROTO_HOST_NAME);
        assert_eq!(consumed, 24, "oversized payload carries the 8-byte annex");
        assert_eq!(hdr.postsize, 0xFFFF);
        assert_eq!(hdr.extended_postsize, Some((big.len() - consumed) as u32));
    }

    /// A rename written to the shared identity slot is reflected in the
    /// next circuit handshake: the CLIENT_NAME frame carries the new user
    /// name, not the env value the slot was seeded with.
    #[test]
    fn handshake_reflects_renamed_identity_slot() {
        let identity = test_identity();
        identity.write().user = "renamed-operator".to_string();
        let hs = build_client_handshake(0, &identity);

        let (vhdr, vconsumed) = CaHeader::from_bytes_extended(&hs).expect("parse VERSION");
        assert_eq!(vhdr.cmmd, CA_PROTO_VERSION);
        let (chdr, cconsumed) =
            CaHeader::from_bytes_extended(&hs[vconsumed..]).expect("parse CLIENT_NAME");
        assert_eq!(chdr.cmmd, crate::protocol::CA_PROTO_CLIENT_NAME);

        let payload_start = vconsumed + cconsumed;
        let payload = &hs[payload_start..payload_start + chdr.postsize as usize];
        let name = String::from_utf8_lossy(payload);
        assert_eq!(name.trim_end_matches('\0'), "renamed-operator");
    }

    /// Spawn a transport manager wired to fresh channels; return its
    /// command sender, event receiver, and the (observable) per-circuit
    /// writer registry.
    fn spawn_manager() -> (
        mpsc::UnboundedSender<TransportCommand>,
        mpsc::UnboundedReceiver<TransportEvent>,
        DirectServerWriters,
    ) {
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let in_flight = InFlightOps::new();
        let server_writers: DirectServerWriters = Arc::new(dashmap::DashMap::new());
        let last_rx_at: ServerLastRxAt = Arc::new(dashmap::DashMap::new());
        let observable = server_writers.clone();
        let identity = test_identity();
        #[cfg(not(feature = "experimental-rust-tls"))]
        tokio::spawn(run_transport_manager(
            cmd_rx,
            event_tx,
            in_flight,
            server_writers,
            last_rx_at,
            identity,
        ));
        #[cfg(feature = "experimental-rust-tls")]
        tokio::spawn(run_transport_manager(
            cmd_rx,
            event_tx,
            in_flight,
            server_writers,
            last_rx_at,
            identity,
            None,
            None,
            HashMap::new(),
        ));
        (cmd_tx, event_rx, observable)
    }

    /// Read the client's full handshake off a freshly accepted server
    /// socket and return the VERSION priority. Draining the whole
    /// handshake (its exact length is `build_client_handshake(pri).len()`
    /// because the test shares this process's USER/host env) leaves the
    /// socket positioned at the next frame, so a later read observes
    /// genuinely new traffic rather than buffered handshake bytes.
    async fn drain_handshake(stream: &mut TcpStream) -> u8 {
        let mut head = [0u8; 16];
        stream
            .read_exact(&mut head)
            .await
            .expect("read VERSION header");
        let hdr = CaHeader::from_bytes(&head).expect("parse VERSION");
        assert_eq!(hdr.cmmd, CA_PROTO_VERSION);
        let pri = hdr.data_type as u8;
        let total = build_client_handshake(pri, &test_identity()).len();
        let mut rest = vec![0u8; total - 16];
        stream
            .read_exact(&mut rest)
            .await
            .expect("drain handshake tail");
        pri
    }

    async fn wait_for_writers(sw: &DirectServerWriters, n: usize) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while sw.len() < n {
            assert!(
                tokio::time::Instant::now() < deadline,
                "expected {n} circuit writers, saw {}",
                sw.len()
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// Test 1: two channels to the same server at different priorities
    /// open two independent transport circuit entries, and the server
    /// sees both priorities on the wire.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn two_priorities_open_two_circuits() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let priorities = Arc::new(tokio::sync::Mutex::new(Vec::<u8>::new()));
        let kept = Arc::new(tokio::sync::Mutex::new(Vec::<TcpStream>::new()));
        let pri_log = priorities.clone();
        let keep = kept.clone();
        let acceptor = tokio::spawn(async move {
            for _ in 0..2 {
                let (mut s, _) = listener.accept().await.unwrap();
                let pri = drain_handshake(&mut s).await;
                pri_log.lock().await.push(pri);
                keep.lock().await.push(s); // hold the socket so the circuit stays up
            }
        });

        let (cmd_tx, _event_rx, sw) = spawn_manager();
        cmd_tx
            .send(TransportCommand::CreateChannel {
                cid: 1,
                pv_name: "X".into(),
                server_addr: addr,
                priority: 0,
            })
            .unwrap();
        cmd_tx
            .send(TransportCommand::CreateChannel {
                cid: 2,
                pv_name: "Y".into(),
                server_addr: addr,
                priority: 7,
            })
            .unwrap();

        wait_for_writers(&sw, 2).await;
        assert!(
            sw.contains_key(&(addr, 0)),
            "priority-0 circuit writer missing"
        );
        assert!(
            sw.contains_key(&(addr, 7)),
            "priority-7 circuit writer missing"
        );
        assert_eq!(
            sw.len(),
            2,
            "two priorities to one server must yield two independent circuits"
        );

        let _ = tokio::time::timeout(Duration::from_secs(5), acceptor).await;
        let mut seen = priorities.lock().await.clone();
        seen.sort_unstable();
        assert_eq!(
            seen,
            vec![0u8, 7],
            "server observed both priorities on the wire"
        );
    }

    /// Test 3: dropping one priority circuit closes only that circuit;
    /// the sibling circuit at another priority stays connected and keeps
    /// carrying frames.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dropping_one_priority_circuit_leaves_the_other() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let (sock_tx, mut sock_rx) = mpsc::unbounded_channel::<(u8, TcpStream)>();
        let acceptor = tokio::spawn(async move {
            for _ in 0..2 {
                let (mut s, _) = listener.accept().await.unwrap();
                let pri = drain_handshake(&mut s).await;
                let _ = sock_tx.send((pri, s));
            }
        });

        let (cmd_tx, mut event_rx, sw) = spawn_manager();
        cmd_tx
            .send(TransportCommand::CreateChannel {
                cid: 1,
                pv_name: "X".into(),
                server_addr: addr,
                priority: 0,
            })
            .unwrap();
        cmd_tx
            .send(TransportCommand::CreateChannel {
                cid: 2,
                pv_name: "Y".into(),
                server_addr: addr,
                priority: 5,
            })
            .unwrap();

        // Collect both server-side sockets, keyed by the priority each
        // negotiated.
        let mut socks: HashMap<u8, TcpStream> = HashMap::new();
        for _ in 0..2 {
            let (pri, s) = tokio::time::timeout(Duration::from_secs(5), sock_rx.recv())
                .await
                .expect("accept timed out")
                .expect("acceptor closed early");
            socks.insert(pri, s);
        }
        wait_for_writers(&sw, 2).await;
        let _ = tokio::time::timeout(Duration::from_secs(2), acceptor).await;

        // Tear down the priority-0 circuit by closing its server socket.
        drop(socks.remove(&0).expect("priority-0 server socket"));

        // The client must report TcpClosed for priority 0 — and NEVER for
        // priority 5.
        let mut saw_zero_closed = false;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while tokio::time::Instant::now() < deadline && !saw_zero_closed {
            match tokio::time::timeout(Duration::from_millis(200), event_rx.recv()).await {
                Ok(Some(TransportEvent::TcpClosed {
                    server_addr,
                    priority,
                })) => {
                    assert_eq!(server_addr, addr);
                    assert_ne!(
                        priority, 5,
                        "priority-5 circuit must not close when priority-0 is dropped"
                    );
                    if priority == 0 {
                        saw_zero_closed = true;
                    }
                }
                Ok(Some(_)) => {} // ServerConnected / ServerVersion / etc.
                Ok(None) => break,
                Err(_) => {} // 200ms tick, keep polling until the deadline
            }
        }
        assert!(
            saw_zero_closed,
            "dropping the priority-0 server socket must emit TcpClosed{{priority:0}}"
        );

        // The priority-5 circuit is still alive: its writer remains, and a
        // fresh frame sent on it actually reaches the server socket.
        assert!(
            sw.contains_key(&(addr, 5)),
            "priority-5 circuit writer must survive the priority-0 teardown"
        );
        cmd_tx
            .send(TransportCommand::ClearChannel {
                cid: 2,
                sid: 0,
                server_addr: addr,
                priority: 5,
            })
            .unwrap();
        let mut s5 = socks.remove(&5).expect("priority-5 server socket");
        let mut frame = [0u8; 16];
        let read = tokio::time::timeout(Duration::from_secs(5), s5.read_exact(&mut frame)).await;
        assert!(
            read.is_ok() && read.unwrap().is_ok(),
            "surviving priority-5 circuit must still carry frames after the sibling closed"
        );
        let _ = s5.shutdown().await;
    }
}
