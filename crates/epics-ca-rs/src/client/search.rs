use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::time::{Duration, Instant};

use epics_base_rs::net::AsyncUdpV4;
use epics_base_rs::runtime::sync::mpsc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::interval;

use crate::protocol::*;

use super::circuit_breaker::CircuitBreakerRegistry;
use super::types::{SearchAttempts, SearchReason, SearchRequest, SearchResponse};
use std::sync::atomic::{AtomicU32, Ordering};

/// Snippet of a UDP/TCP search-response datagram, plus the address it
/// arrived from. Used to feed nameserver TCP responses through the same
/// `handle_udp_response` parser as plain UDP search replies.
type ParsedDatagram = (Vec<u8>, SocketAddr);

/// Send `buf` toward `addr`, expanding to a per-NIC fanout when the
/// destination is the limited broadcast `255.255.255.255` or an IPv4
/// multicast group (`224.0.0.0/4`). Per-subnet broadcasts and
/// unicast destinations route via the NIC chosen by [`AsyncUdpV4`].
async fn send_with_fanout(
    socket: &AsyncUdpV4,
    buf: &[u8],
    addr: SocketAddr,
    site: &'static str,
    send_errors: &mut HashMap<SocketAddr, std::io::ErrorKind>,
) {
    let needs_fanout = match addr {
        SocketAddr::V4(v4) => v4.ip().is_broadcast() || v4.ip().is_multicast(),
        SocketAddr::V6(_) => false,
    };
    let result = if needs_fanout {
        socket.fanout_to(buf, addr).await.map(|_| ())
    } else {
        socket.send_to(buf, addr).await.map(|_| ())
    };
    match result {
        Ok(()) => {
            // libca cae597d: log once-on-recovery so operators know
            // when a broken destination came back.
            if let Some(prev) = send_errors.remove(&addr) {
                tracing::info!(
                    target: "epics_ca_rs::search",
                    %addr, site, prev_error = ?prev,
                    "search send_to: recovered"
                );
            }
        }
        Err(e) => {
            // P-7 + libca cae597d (`udpiiu::SearchDestUDP::_lastError`):
            // log on first occurrence and on error-kind change; suppress
            // repeated identical errors so a persistent EHOSTUNREACH
            // doesn't flood the log at search rate.
            let kind = e.kind();
            let prev = send_errors.insert(addr, kind);
            if prev != Some(kind) {
                tracing::warn!(
                    target: "epics_ca_rs::search",
                    %addr,
                    site,
                    error = %e,
                    "search send_to failed"
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Configuration constants
// ---------------------------------------------------------------------------

/// pvxs `client.cpp::nBuckets`. 30 buckets at 1 s normal interval gives
/// each pending search a 30-second slot rotation — cooperative tick
/// caps UDP search traffic at roughly `pending.len() / 30` packets per
/// second instead of letting every channel fire on its own backoff.
const N_SEARCH_BUCKETS: usize = 30;

/// Decide which bucket to drop a fresh search into based on the
/// caller's intent. Pure function so the production handler and the
/// unit tests share the formula and can't drift apart.
///
/// - `Initial` / `BeaconAnomaly` (new cid): `current_bucket + 1`. The
///   handler ALSO fires an immediate broadcast for `Initial`; the +1
///   placement is so the first scheduled retry lands one tick after
///   the immediate fire. `BeaconAnomaly` for a new cid relies on
///   the engine's fast-tick mode to retransmit within ~6 s, so the
///   +1 placement gets caught by the next fast tick.
/// - `Reconnect`: `current_bucket`. Mirrors pvxs `Channel::disconnect`
///   (client.cpp:213) with `holdoff = 0` — the typical Active→
///   disconnect case sits in the current bucket and the next 1 Hz
///   tick fires the broadcast. Latency ≤ 1 s.
///
/// Cascade-spread (5000 channels disconnecting simultaneously) is
/// handled by the natural O(N / nBuckets) per-tick rate-limit and
/// the runtime-side smoothing in `cascade_smoothed_next` — no
/// per-channel cid hashing needed for the first attempt.
fn placement_bucket(current_bucket: usize, reason: SearchReason) -> usize {
    match reason {
        SearchReason::Initial | SearchReason::BeaconAnomaly => {
            (current_bucket + 1) % N_SEARCH_BUCKETS
        }
        SearchReason::Reconnect => current_bucket,
    }
}

/// Compute the next-retry bucket for a search that just transmitted.
/// Mirrors pvxs `tickSearch` (client.cpp:1193-1206):
///
///   `next = (idx + nSearch) % nBuckets`, where `nSearch` is the
///   per-channel attempt counter, capped at `nBuckets`. Each retry
///   pushes the search forward by one more bucket: 1 s, 2 s, 3 s,
///   ..., capping at the 30 s ring period.
///
/// Cascade smoothing (line 1199-1206 in pvxs): when the chosen
/// `next` bucket is overloaded relative to the bucket immediately
/// after it (>100 entries more), defer to that one. Distributes a
/// mass-disconnect across two ticks instead of one. Threshold is
/// strictly `>` 100, matching pvxs.
///
/// `attempt` is 1-based (1 means "this is the first retransmit
/// after the initial bucket-fire"). The earlier
/// `RETRY_HOLDOFF_CYCLES = 10` mechanism conflated pvxs's pre-
/// CREATE_CHANNEL holdoff (which only applies to the
/// `Channel::Connecting` state) with the steady-state retry
/// cadence; pvxs uses the `nSearch` increment for the latter.
fn cascade_smoothed_next(
    current_bucket: usize,
    attempt: u32,
    bucket_sizes: impl Fn(usize) -> usize,
) -> usize {
    let n_search = (attempt as usize).min(N_SEARCH_BUCKETS);
    let next = (current_bucket + n_search) % N_SEARCH_BUCKETS;
    let nextnext = (next + 1) % N_SEARCH_BUCKETS;
    let next_n = bucket_sizes(next);
    let nextnext_n = bucket_sizes(nextnext);
    if next_n > nextnext_n && next_n - nextnext_n > 100 {
        nextnext
    } else {
        next
    }
}

/// Normal tick cadence (1 search bucket per second).
const NORMAL_TICK: Duration = Duration::from_secs(1);

/// Fast-mode tick cadence after a beacon poke. One full bucket
/// revolution fits in `N_SEARCH_BUCKETS * FAST_TICK = 6 s`.
const FAST_TICK: Duration = Duration::from_millis(200);

/// Maximum bytes per outbound UDP datagram.
const MAX_UDP_SEND: usize = 1024;

/// Penalty hold-off after a failed connect to a server.
const PENALTY_DURATION: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------------------
// Per-channel search state
// ---------------------------------------------------------------------------

struct PendingSearch {
    #[allow(dead_code)]
    cid: u32,
    #[allow(dead_code)]
    pv_name: String,
    /// Pre-built payload: SEARCH header + padded PV name (no VERSION prefix).
    search_payload: Vec<u8>,
    /// Which bucket this search currently lives in.
    bucket: usize,
    /// Number of times this search has been broadcast. 0 before the
    /// first transmit; doubles as the pvxs `nSearch` counter that
    /// controls retry-bucket escalation in `cascade_smoothed_next`
    /// — each retry pushes the search forward by `min(attempt,
    /// nBuckets)` buckets, giving the 1 s, 2 s, 3 s, ..., 30 s
    /// pattern.
    attempt: u32,
    #[allow(dead_code)]
    last_attempt: Option<Instant>,
}

// ---------------------------------------------------------------------------
// Penalty box
// ---------------------------------------------------------------------------

struct PenaltyEntry {
    until: Instant,
}

// ---------------------------------------------------------------------------
// Top-level engine state
// ---------------------------------------------------------------------------

struct SearchEngineState {
    pending: HashMap<u32, PendingSearch>,
    buckets: Vec<Vec<u32>>,
    current_bucket: usize,
    /// Shared per-channel SEARCH attempt counter — bumped by
    /// `fire_searches` on every fanout (immediate first SEARCH AND
    /// each bucket-tick retransmit) so
    /// [`super::CaChannel::search_attempts`] (CA-035) returns the
    /// same number `ca_search_attempts(chid)` returns in libca.
    /// Entry is removed on Cancel and on successful CREATE_CHANNEL
    /// reply (mirrors C reset on circuit attach).
    attempts: SearchAttempts,
    /// After a beacon poke we run one full revolution at FAST_TICK
    /// cadence so all pending searches retry within ~6 s.
    fast_ticks_remaining: u32,
    penalty: HashMap<SocketAddr, PenaltyEntry>,
    /// Per-server failure-pattern tracker. Sits on top of the single-shot
    /// `penalty` box: when failures repeat within a window, the breaker
    /// trips OPEN with an exponentially-doubled cooldown so we don't
    /// hammer a flapping server.
    breakers: CircuitBreakerRegistry,
    /// Sequence number for datagram validation (matches C EPICS
    /// lastReceivedSeqNo).  Embedded in VERSION header CID field;
    /// servers echo it back, letting us reject stale responses.
    dgram_seq: u32,
    /// Last validated sequence number from a VERSION response.
    last_valid_seq: Option<u32>,
    /// Per-destination last UDP send-error kind. Mirrors libca cae597d
    /// (`udpiiu::SearchDestUDP::_lastError`): a persistent sendto()
    /// failure (e.g. firewall, unreachable broadcast) repeats at search
    /// rate (~30 ms) and would otherwise spam logs. We log on first
    /// occurrence, on errno change, and on recovery; suppress repeats.
    send_errors: HashMap<SocketAddr, std::io::ErrorKind>,
    /// `EPICS_RS_CLIENT_IGNORE` filter snapshot taken at startup.
    /// Rust-only client-side extension — NOT the C
    /// `EPICS_IOC_IGNORE_SERVERS` (server-side; see
    /// `client::epics_rs_client_ignore` docstring for the naming
    /// rationale). Any SEARCH reply whose announced server IP — or
    /// the datagram source IP — appears here is dropped before the
    /// per-channel attempt counter is consulted. Held in `HashSet`
    /// for O(1) lookup; updated only at engine start (env changes
    /// mid-run are not picked up to keep the hot path lock-free).
    ignored_servers: std::collections::HashSet<Ipv4Addr>,
}

impl SearchEngineState {
    #[cfg(test)]
    fn new() -> Self {
        Self::with_attempts(std::sync::Arc::new(dashmap::DashMap::new()))
    }

    fn with_attempts(attempts: SearchAttempts) -> Self {
        Self {
            pending: HashMap::new(),
            buckets: (0..N_SEARCH_BUCKETS).map(|_| Vec::new()).collect(),
            current_bucket: 0,
            attempts,
            fast_ticks_remaining: 0,
            penalty: HashMap::new(),
            breakers: CircuitBreakerRegistry::new(),
            dgram_seq: 0,
            last_valid_seq: None,
            send_errors: HashMap::new(),
            ignored_servers: super::epics_rs_client_ignore().into_iter().collect(),
        }
    }

    /// Remove a channel entirely.
    fn remove_channel(&mut self, cid: u32) {
        if let Some(p) = self.pending.remove(&cid) {
            self.buckets[p.bucket].retain(|x| *x != cid);
        }
        self.attempts.remove(&cid);
    }

    /// pvxs `client.cpp:713 poke()` parity: reset every pending
    /// search's attempt + holdoff counters and start the engine's
    /// fast-tick revolution. Searches stay in their assigned buckets;
    /// fast-tick (200 ms) covers the full ring in 6 s so each pending
    /// search retries once within that window.
    fn poke(&mut self) {
        for p in self.pending.values_mut() {
            // NOTE: more aggressive than pvxs's `poked` semantic
            // (which preserves nSearch and just skips its increment
            // for one tick). Resetting attempt to 0 means the
            // post-poke retries cascade from the 1-bucket forward
            // push from scratch — rapid retransmits during the
            // fast-tick window. Acceptable trade for single-channel
            // recovery; under mass-disconnect cascades it spends
            // more UDP bandwidth than pvxs would.
            p.attempt = 0;
            p.last_attempt = None;
        }
        self.fast_ticks_remaining = N_SEARCH_BUCKETS as u32;
    }
}

// ---------------------------------------------------------------------------
// Main entry point
// ---------------------------------------------------------------------------

pub(crate) async fn run_search_engine(
    mut addr_list: Vec<super::AddrEntry>,
    nameserver_addrs: Vec<SocketAddr>,
    mut request_rx: mpsc::UnboundedReceiver<SearchRequest>,
    response_tx: mpsc::UnboundedSender<SearchResponse>,
    attempts: SearchAttempts,
) {
    // libca-style multi-NIC: one bound socket per IPv4 interface so
    // `255.255.255.255` and per-subnet broadcasts each leave via the
    // matching NIC. SO_REUSEADDR + (Linux) IP_MULTICAST_ALL=0 are
    // applied to every per-NIC socket inside `AsyncUdpV4::bind`.
    let socket = match AsyncUdpV4::bind(0, true) {
        Ok(s) => s,
        Err(_) => return,
    };
    // Larger receive buffer absorbs multi-PV SEARCH response bursts.
    let _ = socket.set_recv_buffer_size(256 * 1024);
    // Apply `EPICS_CA_MCAST_TTL` (epics-base 3.16, f2a1834d). Affects
    // outgoing packets only when the destination falls in 224.0.0.0/4;
    // setting it unconditionally is safe and lets sites that
    // multicast SEARCH across routed segments raise the TTL via env.
    let _ = socket.set_multicast_ttl_v4(epics_base_rs::runtime::net::ca_mcast_ttl());
    // pvxs `client.cpp` parity (commit a064677e3625): opt every per-NIC
    // SEARCH socket into SO_RXQ_OVFL so a sustained reply backlog
    // (slow main-loop, undersized SO_RCVBUF, mass-disconnect storm)
    // surfaces as a debug log instead of silent reply loss. No-op on
    // non-Linux. Failure is logged at trace and ignored — the
    // counter is diagnostic-only.
    if let Err(e) = socket.enable_so_rxq_ovfl() {
        tracing::trace!(
            target: "epics_ca_rs::client::search",
            error = %e,
            "SO_RXQ_OVFL enable on per-NIC SEARCH bundle failed (non-fatal)"
        );
    }

    // Spawn a connection task per EPICS_CA_NAME_SERVERS entry.
    // Each task auto-reconnects with exponential backoff and forwards
    // outgoing search bytes to its TCP socket. Incoming responses are
    // queued via tcp_response_tx for the main loop to process through
    // the shared handle_udp_response parser.
    let (tcp_response_tx, mut tcp_response_rx) = mpsc::unbounded_channel::<ParsedDatagram>();
    // Reproducer for Launchpad bug #739789: pre-fix, this was an
    // unbounded mpsc — when the nameserver TCP socket was unresponsive
    // the per-tick search frames piled up indefinitely (each frame
    // ~MAX_UDP_SEND bytes), eventually consuming process memory. Use
    // a bounded mpsc so a stuck TCP peer drops messages instead of
    // leaking. Cap is per-nameserver, not global. Override via
    // EPICS_CA_NAMESERVER_QUEUE_DEPTH; default 256 is large enough to
    // ride out a few-second TCP stall without observable search loss
    // and small enough to bound RSS at a few MB worst-case.
    let ns_queue_cap = epics_base_rs::runtime::env::get("EPICS_CA_NAMESERVER_QUEUE_DEPTH")
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(256)
        .max(8);
    let mut nameserver_send_txs: Vec<mpsc::Sender<Vec<u8>>> = Vec::new();
    for addr in nameserver_addrs {
        let (tx, rx) = mpsc::channel::<Vec<u8>>(ns_queue_cap);
        nameserver_send_txs.push(tx);
        let resp_tx = tcp_response_tx.clone();
        epics_base_rs::runtime::task::spawn(async move {
            run_nameserver_connection(addr, rx, resp_tx).await;
        });
    }

    let mut state = SearchEngineState::with_attempts(attempts);
    let mut recv_buf = [0u8; 65536];
    // pvxs parity: track per-NIC SO_RXQ_OVFL counters, log on
    // transitions only. Key on the receiving NIC's iface_ip
    // (already exposed on the AsyncUdpV4 RecvMeta we read each tick).
    let mut prev_drops_per_iface: HashMap<Ipv4Addr, u32> = HashMap::new();

    // pvxs `client.cpp::tickSearch`: a single steady tick advances the
    // bucket cursor. fast_tick is engaged after a beacon poke for one
    // full revolution, then we revert to NORMAL_TICK.
    let mut tick = interval(NORMAL_TICK);
    tick.tick().await; // skip immediate fire
    let mut tick_is_fast = false;

    // Round 50 (R50-G2): periodic DNS refresh for `EPICS_CA_ADDR_LIST`
    // entries whose `hostname` was set at startup (i.e. non-IP-literal
    // entries). On each tick the engine walks `addr_list` and calls
    // `AddrEntry::refresh_dns`; a changed resolution updates the
    // entry's `sock` so subsequent `fire_searches` use the new IP.
    // Period is operator-tunable via `EPICS_CA_DNS_REFRESH_SECS`;
    // default 60 s balances responsiveness against DNS load. Literal
    // IP entries (`hostname == None`) short-circuit inside
    // `refresh_dns` so the cost is bounded by hostname count.
    let dns_refresh_secs: u64 = epics_base_rs::runtime::env::get("EPICS_CA_DNS_REFRESH_SECS")
        .and_then(|s| s.parse().ok())
        .filter(|&n: &u64| n > 0)
        .unwrap_or(60);
    let mut dns_refresh = interval(Duration::from_secs(dns_refresh_secs));
    dns_refresh.tick().await; // skip immediate fire

    loop {
        tokio::select! {
            req = request_rx.recv() => {
                let Some(req) = req else { return };
                let mut immediate: Vec<u32> = Vec::new();
                if let Some(cid) = handle_request_or_addr(&mut state, &mut addr_list, req) {
                    immediate.push(cid);
                }
                // Drain any additional queued requests so a burst of
                // Schedule messages all land before the next tick.
                while let Ok(req) = request_rx.try_recv() {
                    if let Some(cid) = handle_request_or_addr(&mut state, &mut addr_list, req) {
                        immediate.push(cid);
                    }
                }
                // pvxs `clientdiscover.cpp` parity: send the first SEARCH
                // packet right now instead of waiting up to one tick for
                // the bucket to come around. The bucket placement still
                // governs all subsequent retries.
                if !immediate.is_empty() {
                    fire_searches(&mut state, &immediate, &addr_list, &socket, &nameserver_send_txs).await;
                }
            }

            result = socket.recv_with_meta_with_drops(&mut recv_buf) => {
                let Ok((meta, drops)) = result else { continue };
                // Surface per-NIC kernel drop transitions — pvxs
                // `udp_collector.cpp:55-67` logs at debug on
                // `prev != current && current != 0`.
                let prev = prev_drops_per_iface.insert(meta.iface_ip, drops).unwrap_or(0);
                if drops != 0 && drops != prev {
                    tracing::debug!(
                        target: "epics_ca_rs::client::search",
                        iface_ip = %meta.iface_ip,
                        prev,
                        drops,
                        "CA client SEARCH per-NIC socket buffer overflow"
                    );
                }
                handle_udp_response(&mut state, &recv_buf[..meta.n], meta.src, &response_tx);
            }

            tcp_dgram = tcp_response_rx.recv() => {
                let Some((bytes, src)) = tcp_dgram else { continue };
                handle_udp_response(&mut state, &bytes, src, &response_tx);
            }

            _ = tick.tick() => {
                process_bucket(&mut state, &addr_list, &socket, &nameserver_send_txs).await;
                if state.fast_ticks_remaining > 0 {
                    state.fast_ticks_remaining -= 1;
                }
            }

            _ = dns_refresh.tick() => {
                // R50-G2: re-resolve every hostname entry. The
                // `refresh_dns()` call is a no-op for IP-literal
                // entries; for DNS entries it does a fresh
                // `to_socket_addrs()` and replaces the cached IP
                // when it differs. We log changes at info-level so
                // operators can correlate an IOC migration with
                // the client's discovery of the new address.
                for entry in addr_list.iter_mut() {
                    let prev_sock = entry.sock;
                    match entry.refresh_dns() {
                        Ok(new_sock) if new_sock != prev_sock => {
                            tracing::info!(
                                hostname = ?entry.hostname,
                                old = %prev_sock,
                                new = %new_sock,
                                "ca-rs: EPICS_CA_ADDR_LIST entry re-resolved"
                            );
                        }
                        Ok(_) => {}
                        Err(e) => {
                            tracing::debug!(
                                hostname = ?entry.hostname,
                                error = %e,
                                "ca-rs: DNS refresh failed; keeping cached IP"
                            );
                        }
                    }
                }
            }
        }

        // Tick-cadence transitions are evaluated outside the select! arm so
        // every event path (Schedule, response, tick) gets the same chance
        // to flip the engine in/out of fast mode based on the current
        // `fast_ticks_remaining`.
        if state.fast_ticks_remaining > 0 && !tick_is_fast {
            tick = interval(FAST_TICK);
            tick.tick().await; // skip immediate fire
            tick_is_fast = true;
        } else if state.fast_ticks_remaining == 0 && tick_is_fast {
            tick = interval(NORMAL_TICK);
            tick.tick().await; // skip immediate fire
            tick_is_fast = false;
        }
    }
}

/// Long-lived task: maintain a TCP connection to one nameserver, forward
/// outgoing search bytes from `outgoing_rx`, and feed parsed response
/// frames into `response_tx`. Reconnects with exponential backoff on
/// failure.
async fn run_nameserver_connection(
    addr: SocketAddr,
    mut outgoing_rx: mpsc::Receiver<Vec<u8>>,
    response_tx: mpsc::UnboundedSender<ParsedDatagram>,
) {
    let mut backoff = Duration::from_secs(1);
    let max_backoff = Duration::from_secs(30);

    loop {
        let stream =
            match tokio::time::timeout(Duration::from_secs(5), TcpStream::connect(addr)).await {
                Ok(Ok(s)) => s,
                _ => {
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(max_backoff);
                    continue;
                }
            };
        let _ = stream.set_nodelay(true);
        backoff = Duration::from_secs(1);

        let (mut reader, mut writer) = stream.into_split();

        // Send initial VERSION + HOST_NAME + CLIENT_NAME so the nameserver
        // accepts our search frames (mirrors transport.rs handshake).
        let mut handshake = Vec::new();
        let mut version = CaHeader::new(CA_PROTO_VERSION);
        version.count = CA_MINOR_VERSION;
        handshake.extend_from_slice(&version.to_bytes());
        let host_payload = pad_string(&epics_base_rs::runtime::env::hostname());
        let mut host = CaHeader::new(CA_PROTO_HOST_NAME);
        host.postsize = host_payload.len() as u16;
        handshake.extend_from_slice(&host.to_bytes());
        handshake.extend_from_slice(&host_payload);
        let user = epics_base_rs::runtime::env::get("USER")
            .or_else(|| epics_base_rs::runtime::env::get("USERNAME"))
            .unwrap_or_else(|| "unknown".to_string());
        let user_payload = pad_string(&user);
        let mut client = CaHeader::new(CA_PROTO_CLIENT_NAME);
        client.postsize = user_payload.len() as u16;
        handshake.extend_from_slice(&client.to_bytes());
        handshake.extend_from_slice(&user_payload);
        if writer.write_all(&handshake).await.is_err() {
            tokio::time::sleep(backoff).await;
            continue;
        }

        let resp_tx = response_tx.clone();
        let read_task = epics_base_rs::runtime::task::spawn(async move {
            let mut buf = vec![0u8; 8192];
            let mut accumulated: Vec<u8> = Vec::new();
            loop {
                let n = match reader.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => n,
                };
                accumulated.extend_from_slice(&buf[..n]);
                // Forward only the prefix that contains complete CA
                // messages. Without this framing, kernel splitting a
                // server response across read syscalls causes the
                // dispatcher to miss leading frames (when the partial
                // buffer is < 16 bytes) and misalign subsequent
                // parses. Each CA message is 16-byte header +
                // align8(postsize) — no extended-postsize support
                // here because the dispatcher itself ignores it.
                let mut consumed = 0usize;
                loop {
                    if accumulated.len() - consumed < CaHeader::SIZE {
                        break;
                    }
                    // CR-11: handle extended postsize (postsize=0xFFFF,
                    // count=0 → 8 extra header bytes + true u32 size).
                    // Pure 16-byte parse would consume 65,540 bytes for
                    // a frame whose true size is 24 + payload.
                    let (hdr, hdr_size) =
                        match CaHeader::from_bytes_extended(&accumulated[consumed..]) {
                            Ok(v) => v,
                            Err(_) => break,
                        };
                    let actual_post = hdr.actual_postsize();
                    // C `tcpiiu.cpp::processIncoming:1198` rejects
                    // misaligned `m_postsize`. Closing the connection
                    // is the only safe action — silently rounding up
                    // (the prior `align8`) would let a hostile name
                    // server slide our framer into the middle of the
                    // next message. Break the read loop; the
                    // surrounding reconnect path will rebuild.
                    if actual_post & 0x7 != 0 {
                        break;
                    }
                    let msg_size = hdr_size + actual_post;
                    if accumulated.len() - consumed < msg_size {
                        break;
                    }
                    consumed += msg_size;
                }
                if consumed > 0 {
                    let frame_bytes = accumulated[..consumed].to_vec();
                    let _ = resp_tx.send((frame_bytes, addr));
                    accumulated.drain(..consumed);
                }
            }
        });

        // Pipe outgoing search frames to the TCP writer until the reader
        // task ends or the channel closes.
        let mut writer_failed = false;
        // Closed outgoing channel = client shutdown. Track it so we
        // fall through to read_task cleanup, then exit the outer
        // reconnect loop. Earlier code `return`-ed directly which
        // skipped the cleanup and leaked the read task per
        // nameserver on every shutdown.
        let mut shutdown = false;
        'pump: loop {
            tokio::select! {
                msg = outgoing_rx.recv() => {
                    let Some(bytes) = msg else {
                        shutdown = true;
                        break 'pump;
                    };
                    if writer.write_all(&bytes).await.is_err() {
                        writer_failed = true;
                        break 'pump;
                    }
                }
                _ = epics_base_rs::runtime::task::sleep(Duration::from_secs(60)) => {
                    // Periodic noop keeps the connection warm.
                    let echo = CaHeader::new(CA_PROTO_ECHO);
                    if writer.write_all(&echo.to_bytes()).await.is_err() {
                        writer_failed = true;
                        break 'pump;
                    }
                }
            }
            if read_task.is_finished() {
                break 'pump;
            }
        }
        read_task.abort();
        let _ = read_task.await;

        if shutdown {
            // Outgoing channel closed → no more senders ever → don't
            // reconnect; exit the per-nameserver task.
            return;
        }

        if writer_failed {
            // Brief pause before reconnect to avoid a spin loop when the
            // nameserver is fully unreachable.
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(max_backoff);
        }
    }
}

// ---------------------------------------------------------------------------
// Request handling
// ---------------------------------------------------------------------------

/// Wrapper that handles the address-list mutation variants
/// inline (they need mutable access to `addr_list` which
/// `handle_request` doesn't have) and delegates everything else.
///
/// Round 50 (R50-G2): `addr_list` is `Vec<AddrEntry>` so the
/// engine carries the original hostname (if any) for DNS
/// re-resolution. Programmatic adds via `SearchRequest::AddAddress`
/// arrive as `SocketAddr` (no hostname context) and are wrapped
/// as `AddrEntry` with `hostname=None` — they're effectively IP
/// literals on the wire.
fn handle_request_or_addr(
    state: &mut SearchEngineState,
    addr_list: &mut Vec<super::AddrEntry>,
    req: SearchRequest,
) -> Option<u32> {
    match req {
        SearchRequest::AddAddress(addr) => {
            if !addr_list.iter().any(|e| e.sock == addr) {
                let port = match addr {
                    SocketAddr::V4(a) => a.port(),
                    SocketAddr::V6(a) => a.port(),
                };
                addr_list.push(super::AddrEntry::new(addr, None, port));
                tracing::info!(?addr, "ca-rs: addr_list += (programmatic)");
            }
            None
        }
        SearchRequest::SetAddressList(list) => {
            tracing::info!(count = list.len(), "ca-rs: addr_list replaced");
            *addr_list = list
                .into_iter()
                .map(|sock| {
                    let port = match sock {
                        SocketAddr::V4(a) => a.port(),
                        SocketAddr::V6(a) => a.port(),
                    };
                    super::AddrEntry::new(sock, None, port)
                })
                .collect();
            None
        }
        other => handle_request(state, other),
    }
}

/// Process a search request. Returns `Some(cid)` when the new entry
/// needs an immediate first-attempt SEARCH packet sent (matches pvxs
/// `clientdiscover.cpp` immediate-broadcast on Find). The bucket
/// scheduler controls only retries; without immediate fire the first
/// attempt waits up to one full tick, which is the gap that made
/// ca-rs single-channel reconnect feel slower than pva-rs.
///
/// `None` means no immediate fire — either the request didn't add a
/// new pending entry (Cancel / ConnectResult) or it was a BeaconAnomaly
/// poke for an already-pending channel (counters reset only; fast-tick
/// mode handles the retransmit).
fn handle_request(state: &mut SearchEngineState, req: SearchRequest) -> Option<u32> {
    match req {
        SearchRequest::Schedule {
            cid,
            pv_name,
            reason,
        } => {
            // pvxs `poke()` semantic: BeaconAnomaly for an ALREADY-pending
            // channel must NOT move it to a new bucket. The whole point of
            // bucket distribution is lost if a mass-anomaly piles every
            // pending search into bucket=current+1. Just reset its retry
            // counters and engage fast-tick mode; the search fires within
            // ~6 s when its existing bucket comes around in fast cadence.
            if reason == SearchReason::BeaconAnomaly && state.pending.contains_key(&cid) {
                if let Some(p) = state.pending.get_mut(&cid) {
                    p.attempt = 0;
                    p.last_attempt = None;
                }
                state.fast_ticks_remaining = N_SEARCH_BUCKETS as u32;
                return None;
            }

            let search_payload = build_search_payload(cid, &pv_name);

            // Drop any stale entry before re-scheduling.
            state.remove_channel(cid);

            // Bucket placement (pvxs `Channel::disconnect` parity):
            // Initial / BeaconAnomaly land in `current+1` and pair
            // with an immediate broadcast or fast-tick retransmit;
            // Reconnect lands in `current_bucket` so the very next
            // 1-Hz tick fires it (≤ 1 s reconnect latency). The
            // earlier `(current+1+cid%30)` Reconnect formula gave
            // 1-30 s reconnect latency that combined with the
            // channel layer's wait-for-Found path made ca-rs
            // reconnect feel slower than pva-rs; the comment at
            // the top of `handle_request` flagged this gap. See
            // `placement_bucket` for the full rationale.
            let bucket = placement_bucket(state.current_bucket, reason);
            let p = PendingSearch {
                cid,
                pv_name,
                search_payload,
                bucket,
                attempt: 0,
                last_attempt: None,
            };
            state.buckets[bucket].push(cid);
            state.pending.insert(cid, p);

            if reason == SearchReason::BeaconAnomaly {
                state.poke();
            }

            // Immediate first-attempt SEARCH only on `Initial` (typical
            // single-channel `find()`). Skipping it for `Reconnect` is the
            // whole point of the cid-hashed bucket spread above — without
            // this gate a TCP-close affecting N channels would batch N
            // immediate sends from the main loop's `try_recv` drain
            // (`fire_searches` at the top of `run`), defeating the spread
            // and producing the very burst the bucket scheduler exists to
            // avoid. `BeaconAnomaly` for a NEW cid likewise relies on
            // fast-tick mode (`poke()` above) to retransmit within ~6 s
            // instead of firing right away.
            match reason {
                SearchReason::Initial => Some(cid),
                SearchReason::Reconnect | SearchReason::BeaconAnomaly => None,
            }
        }

        SearchRequest::Cancel { cid } => {
            state.remove_channel(cid);
            None
        }

        SearchRequest::ConnectResult {
            cid,
            success,
            server_addr,
        } => {
            if success {
                state.remove_channel(cid);
                state.penalty.remove(&server_addr);
                state.breakers.record_success(server_addr);
            } else {
                state.penalty.insert(
                    server_addr,
                    PenaltyEntry {
                        until: Instant::now() + PENALTY_DURATION,
                    },
                );
                let was_open = state.breakers.is_open(server_addr);
                state.breakers.record_failure(server_addr);
                if !was_open && state.breakers.is_open(server_addr) {
                    tracing::warn!(server = %server_addr, "circuit breaker tripped OPEN");
                    metrics::counter!("ca_client_circuit_breaker_open_total",
                        "server" => server_addr.to_string())
                    .increment(1);
                }
            }
            None
        }
        // Address-list variants are intercepted by
        // `handle_request_or_addr` before they reach this match.
        // Defensive no-op so adding new variants doesn't crash if
        // future code paths plumb them straight to handle_request.
        SearchRequest::AddAddress(_) | SearchRequest::SetAddressList(_) => None,
    }
}

// ---------------------------------------------------------------------------
// UDP response handling
// ---------------------------------------------------------------------------

fn handle_udp_response(
    state: &mut SearchEngineState,
    data: &[u8],
    src: SocketAddr,
    response_tx: &mpsc::UnboundedSender<SearchResponse>,
) {
    if data.len() < CaHeader::SIZE {
        return;
    }

    let recv_time = Instant::now();
    let mut offset = 0;

    while offset + CaHeader::SIZE <= data.len() {
        let Ok(hdr) = CaHeader::from_bytes(&data[offset..]) else {
            break;
        };

        // C `rsrv/camessage.c:2452` rejects misaligned `m_postsize`.
        // For UDP (where this loop runs), C silently drops the
        // datagram without emitting an error — we do the same by
        // breaking out of the chained-message parse. Without this
        // guard, the `align8(postsize)` advancement would walk into
        // the middle of the next message and stale parses would
        // poison search/beacon state.
        if (hdr.postsize as usize) & 0x7 != 0 {
            break;
        }

        match hdr.cmmd {
            CA_PROTO_VERSION => {
                // Any VERSION in the datagram marks subsequent SEARCH
                // responses as fresh.  If the server echoed our
                // sequenceNoIsValid flag, record the exact seq_no.
                if hdr.data_type & 0x8000 != 0 {
                    state.last_valid_seq = Some(hdr.cid);
                } else {
                    // Server didn't echo our seq — still accept
                    // responses in this datagram (older servers,
                    // or our own Rust IOC, don't echo the flag).
                    state.last_valid_seq = Some(0);
                }
                offset += CaHeader::SIZE + align8(hdr.postsize as usize);
                continue;
            }
            CA_PROTO_SEARCH => {
                let server_port = hdr.data_type;
                // CA v4.8+: cid contains server IP. Both 0 (INADDR_ANY)
                // and 0xFFFFFFFF (~0u32, libca's "address unknown" sentinel
                // — see udpiiu.cpp searchRespAction) mean "use UDP source
                // address". Without handling both, real C softIoc replies
                // (cid=~0u32) get rerouted to 255.255.255.255 and the
                // search appears to fail.
                let server_ip = if hdr.cid == 0 || hdr.cid == u32::MAX {
                    src.ip()
                } else {
                    std::net::IpAddr::V4(Ipv4Addr::from(hdr.cid.to_be_bytes()))
                };
                metrics::counter!("ca_client_search_responses_total").increment(1);
                let server_addr = SocketAddr::new(server_ip, server_port as u16);
                let cid = hdr.available;

                // EPICS_RS_CLIENT_IGNORE: drop SEARCH replies
                // announcing a quarantined server so a beacon-
                // discovered server can't sneak past the
                // EPICS_CA_ADDR_LIST filter. Both the announced
                // server IP and the source IP are checked — most
                // upstream IOCs announce ~0 ("use UDP src"), but a
                // misconfigured server announcing its own IP must
                // also be filtered. Rust-only extension; NOT the C
                // EPICS_IOC_IGNORE_SERVERS — see
                // client::epics_rs_client_ignore docstring.
                if let std::net::IpAddr::V4(v4) = server_ip {
                    if state.ignored_servers.contains(&v4) {
                        offset += CaHeader::SIZE + align8(hdr.postsize as usize);
                        continue;
                    }
                }
                if let std::net::IpAddr::V4(v4) = src.ip() {
                    if state.ignored_servers.contains(&v4) {
                        offset += CaHeader::SIZE + align8(hdr.postsize as usize);
                        continue;
                    }
                }

                // Check penalty box — skip penalized servers so the channel
                // can potentially find a non-penalized one.
                let penalized = state
                    .penalty
                    .get(&server_addr)
                    .map(|p| p.until > recv_time)
                    .unwrap_or(false);

                // Circuit breaker OPEN → reject responses from this server
                // entirely. allow() also performs OPEN→HALF_OPEN transition
                // when the cooldown has elapsed, permitting one probe.
                let breaker_blocked = !state.breakers.allow(server_addr);

                if penalized || breaker_blocked {
                    // Don't consume this response — let the channel keep
                    // searching for a better server.
                    offset += CaHeader::SIZE + align8(hdr.postsize as usize);
                    continue;
                }

                // Reject stale responses from previous search rounds.
                // A valid VERSION with our sequence must precede SEARCH
                // responses in the same datagram.
                if state.last_valid_seq.is_none() {
                    offset += CaHeader::SIZE + align8(hdr.postsize as usize);
                    continue;
                }

                if let Some(p) = state.pending.remove(&cid) {
                    state.buckets[p.bucket].retain(|x| *x != cid);
                    tracing::debug!(
                        pv = %p.pv_name, cid, server = %server_addr,
                        "PV search resolved"
                    );
                    let _ = response_tx.send(SearchResponse::Found { cid, server_addr });
                }
            }
            CA_PROTO_NOT_FOUND => {
                // Server explicitly told us the PV is not on it. We don't
                // remove the channel — another server in the addr list may
                // still answer Found.
            }
            _ => {}
        }

        offset += CaHeader::SIZE + align8(hdr.postsize as usize);
    }
}

// ---------------------------------------------------------------------------
// Per-tick bucket processing
// ---------------------------------------------------------------------------

/// Process exactly one search bucket. Each pending in this bucket
/// gets a UDP retransmit and is then re-armed into a future bucket
/// using pvxs's `nSearch+1` escalation (`tickSearch` line 1193-1196):
///
/// ```text
/// next = (idx + min(attempt, nBuckets)) % nBuckets
/// ```
///
/// `attempt` is bumped immediately after the send so the first
/// retry lands at idx+1 (1 s later), the second at idx+2 (2 s
/// after that), the third at idx+3 (4 s total), …, capping at
/// idx+30 (one full ring = 30 s steady-state). The earlier
/// `holdoff_cycles=10` design conflated pvxs's pre-CREATE_CHANNEL
/// holdoff with the Active-disconnect retry path; pvxs only uses
/// the 10-bucket holdoff for `Channel::Connecting` drops, never
/// for the steady reconnect cadence.
///
/// Cascade smoothing: when the chosen `next` bucket is overloaded
/// vs `next+1` by 100+ entries, defer to `next+1` (mirrors pvxs
/// `client.cpp:1199-1206`). Lets a mass-disconnect spread across
/// two ticks instead of one.
///
/// Steady-state UDP search load = O(1) datagrams per tick regardless
/// of how many channels are pending — the bucket distributes load
/// across the ring. The previous lane-based scheduler had every channel
/// fire on its own deadline and relied on AIMD to dampen storms after
/// the fact; the bucket scheduler prevents storms by construction.
async fn process_bucket(
    state: &mut SearchEngineState,
    addr_list: &[super::AddrEntry],
    socket: &AsyncUdpV4,
    nameserver_txs: &[mpsc::Sender<Vec<u8>>],
) {
    let now = Instant::now();

    // Expire old penalties.
    state.penalty.retain(|_, entry| entry.until > now);

    let current = state.current_bucket;
    let bucket_ids = std::mem::take(&mut state.buckets[current]);

    let mut to_send: Vec<u32> = Vec::new();
    {
        // Split-borrow `pending` and `buckets` so cascade_smoothed_next
        // (which only reads bucket sizes via a closure capture of
        // `&buckets`) can run inline with the per-sid push back to
        // `&mut buckets[next]`. Without the split, the closure's
        // immutable borrow of `state.buckets` would conflict with
        // the subsequent mutable access — which is why the prior
        // version had to batch the rearm into a Vec and apply it
        // post-loop. That batching defeated the within-tick
        // smoothing benefit: a 5000-channel mass-disconnect saw
        // delta=0 for every sid (all 5000 saw an empty `next`
        // bucket because nothing was pushed yet) and piled into
        // `current+1`. With inline push the second sid sees the
        // first's buildup, the third sees two, etc., so smoothing
        // kicks in around the 100-entry boundary just like pvxs's
        // tickSearch line 1199-1206. PVA-rs uses the equivalent
        // pattern where `pending` and `search_buckets` are
        // top-level locals; here we recover the same effect via
        // explicit split-borrow.
        let pending = &mut state.pending;
        let buckets = &mut state.buckets;
        for sid in bucket_ids {
            let Some(p) = pending.get_mut(&sid) else {
                continue;
            };
            p.last_attempt = Some(now);
            p.attempt = p.attempt.saturating_add(1);
            let attempt = p.attempt;
            // Diagnostic counter (CaChannel::search_attempts) is bumped
            // by fire_searches when the SEARCH actually goes on the
            // wire — covers both this bucket-tick path AND the
            // immediate-fire path right after Schedule (which never
            // reaches process_bucket).
            to_send.push(sid);

            let bucket_sizes = |idx: usize| buckets[idx].len();
            let next = cascade_smoothed_next(current, attempt, bucket_sizes);
            // Closure dropped at `cascade_smoothed_next` return —
            // immutable borrow on `buckets` is gone, so the
            // mutable accesses below compile.
            if let Some(p) = pending.get_mut(&sid) {
                p.bucket = next;
            }
            buckets[next].push(sid);
        }
    }

    state.current_bucket = (state.current_bucket + 1) % N_SEARCH_BUCKETS;

    if to_send.is_empty() {
        return;
    }

    fire_searches(state, &to_send, addr_list, socket, nameserver_txs).await;
}

/// Build batched UDP SEARCH datagrams for `cids` and send via every
/// destination + nameserver channel. One VERSION header per datagram
/// carries the rolling sequence number so stale responses are
/// rejected (matches C EPICS dgSeqNoAtTimerExpire). Used both by the
/// per-tick bucket processor and by the immediate-fire path that
/// runs right after handle_request to avoid the up-to-1-tick wait
/// on the first attempt.
async fn fire_searches(
    state: &mut SearchEngineState,
    cids: &[u32],
    addr_list: &[super::AddrEntry],
    socket: &AsyncUdpV4,
    nameserver_txs: &[mpsc::Sender<Vec<u8>>],
) {
    state.dgram_seq = state.dgram_seq.wrapping_add(1);
    let version_hdr = {
        let mut h = CaHeader::new(CA_PROTO_VERSION);
        h.count = CA_MINOR_VERSION;
        h.data_type = 0x8000;
        h.cid = state.dgram_seq;
        h.to_bytes()
    };

    // Build batched UDP datagrams (multi-search per packet, MTU-bounded).
    // Bucket distribution caps per-tick load at ~pending/N_SEARCH_BUCKETS,
    // so no AIMD throttling is needed.
    let mut current_frame = Vec::with_capacity(MAX_UDP_SEND);
    current_frame.extend_from_slice(&version_hdr);

    for sid in cids {
        let Some(p) = state.pending.get(sid) else {
            continue;
        };
        let payload = p.search_payload.clone();
        // CA-035 diagnostic counter: bump per-cid each time we
        // commit to fanning a SEARCH out. Single fire_searches call
        // == one logical attempt for the cid regardless of how many
        // UDP datagrams the addr_list / nameserver fanout produces
        // (matches libca ca_search_attempts(chid) "attempt" semantic).
        // Use fetch_add so beacon poke (which resets p.attempt to 0)
        // does NOT make this counter regress.
        state
            .attempts
            .entry(*sid)
            .or_insert_with(|| AtomicU32::new(0))
            .fetch_add(1, Ordering::Relaxed);

        if current_frame.len() + payload.len() > MAX_UDP_SEND
            && current_frame.len() > CaHeader::SIZE
        {
            for entry in addr_list {
                send_with_fanout(
                    socket,
                    &current_frame,
                    entry.sock,
                    "bucket",
                    &mut state.send_errors,
                )
                .await;
            }
            for ns_tx in nameserver_txs {
                ns_try_send(ns_tx, current_frame.clone());
            }
            current_frame.clear();
            current_frame.extend_from_slice(&version_hdr);
        }

        if CaHeader::SIZE + payload.len() > MAX_UDP_SEND {
            // Single payload exceeds MTU — solo send.
            let mut solo = Vec::with_capacity(CaHeader::SIZE + payload.len());
            solo.extend_from_slice(&version_hdr);
            solo.extend_from_slice(&payload);
            for entry in addr_list {
                send_with_fanout(socket, &solo, entry.sock, "solo", &mut state.send_errors).await;
            }
            for ns_tx in nameserver_txs {
                ns_try_send(ns_tx, solo.clone());
            }
        } else {
            current_frame.extend_from_slice(&payload);
        }
    }

    // Flush the final frame.
    if current_frame.len() > CaHeader::SIZE {
        for entry in addr_list {
            send_with_fanout(
                socket,
                &current_frame,
                entry.sock,
                "flush",
                &mut state.send_errors,
            )
            .await;
        }
        for ns_tx in nameserver_txs {
            ns_try_send(ns_tx, current_frame.clone());
        }
    }
}

/// Drop-on-full helper for nameserver TCP send queues. Mirrors libca
/// behavior under TCP stall: bounded queue, drop excess, log + bump
/// the metric so operators can see queue pressure. Lp #739789.
fn ns_try_send(ns_tx: &mpsc::Sender<Vec<u8>>, frame: Vec<u8>) {
    use tokio::sync::mpsc::error::TrySendError;
    match ns_tx.try_send(frame) {
        Ok(()) => {}
        Err(TrySendError::Full(_)) => {
            metrics::counter!("ca_client_nameserver_queue_drops_total").increment(1);
            tracing::warn!(
                "EPICS_CA_NAME_SERVERS queue full — dropping search frame \
                 (peer is slow/unresponsive; raise EPICS_CA_NAMESERVER_QUEUE_DEPTH \
                 if the peer is healthy)"
            );
        }
        Err(TrySendError::Closed(_)) => {
            // Receiver task exited — nothing more we can do here.
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Build per-channel search payload (SEARCH header + padded PV name).
/// Does NOT include the VERSION header — that is prepended once per datagram.
fn build_search_payload(cid: u32, pv_name: &str) -> Vec<u8> {
    let pv_payload = pad_string(pv_name);

    let mut search_hdr = CaHeader::new(CA_PROTO_SEARCH);
    search_hdr.postsize = pv_payload.len() as u16;
    search_hdr.data_type = CA_DO_REPLY;
    search_hdr.count = CA_MINOR_VERSION;
    search_hdr.cid = cid;
    search_hdr.available = cid;

    let mut payload = Vec::with_capacity(CaHeader::SIZE + pv_payload.len());
    payload.extend_from_slice(&search_hdr.to_bytes());
    payload.extend_from_slice(&pv_payload);
    payload
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn schedule_initial(state: &mut SearchEngineState, cid: u32, pv_name: &str) {
        handle_request(
            state,
            SearchRequest::Schedule {
                cid,
                pv_name: pv_name.to_string(),
                reason: SearchReason::Initial,
            },
        );
    }

    /// Reproducer for Launchpad bug #739789 (TCP nameserver send queue
    /// memory leak): a stuck/slow TCP peer caused libca's `sendQue` to
    /// grow unbounded as the UDP search agent kept pushing frames.
    /// In epics-rs the nameserver-send channel is now bounded via
    /// `EPICS_CA_NAMESERVER_QUEUE_DEPTH` (default 256), and
    /// `ns_try_send` drops the frame instead of blocking or queuing.
    /// This test exercises the helper directly: with a 2-slot channel
    /// and no consumer, the third send must drop.
    #[tokio::test]
    async fn nameserver_queue_drops_when_full_no_leak() {
        let (tx, mut rx) = mpsc::channel::<Vec<u8>>(2);
        ns_try_send(&tx, vec![1, 2, 3]);
        ns_try_send(&tx, vec![4, 5, 6]);
        // Capacity is exhausted — third call must drop, not block.
        ns_try_send(&tx, vec![7, 8, 9]);
        // Drain: only the first two frames are present. The third was
        // dropped, not queued — that is the regression guard.
        assert_eq!(rx.try_recv().unwrap(), vec![1, 2, 3]);
        assert_eq!(rx.try_recv().unwrap(), vec![4, 5, 6]);
        assert!(
            rx.try_recv().is_err(),
            "third frame must be dropped, not queued (lp #739789)"
        );
    }

    #[tokio::test]
    async fn nameserver_queue_handles_closed_receiver() {
        // Receiver dropped — ns_try_send must not panic.
        let (tx, rx) = mpsc::channel::<Vec<u8>>(2);
        drop(rx);
        ns_try_send(&tx, vec![1, 2, 3]);
        // Reaching this line means the call did not panic.
    }

    #[test]
    fn build_search_payload_size() {
        let payload = build_search_payload(42, "TEST:PV");
        // CaHeader::SIZE (16) + pad_string("TEST:PV") = 16 + 8 = 24
        assert_eq!(payload.len(), 24);
    }

    #[test]
    fn build_search_payload_alignment() {
        let payload = build_search_payload(1, "A");
        // pad_string("A") = 8 bytes (1 char + null + 6 padding)
        assert_eq!(payload.len(), CaHeader::SIZE + 8);
        assert_eq!(payload.len() % 8, 0);
    }

    #[test]
    fn schedule_places_into_next_bucket() {
        let mut state = SearchEngineState::new();
        state.current_bucket = 5;
        schedule_initial(&mut state, 1, "PV:1");
        let p = state.pending.get(&1).unwrap();
        assert_eq!(p.bucket, 6);
        assert_eq!(state.buckets[6], vec![1]);
        assert_eq!(state.buckets[5], Vec::<u32>::new());
    }

    #[test]
    fn cancel_removes_from_bucket() {
        let mut state = SearchEngineState::new();
        schedule_initial(&mut state, 1, "PV:1");
        let bucket = state.pending.get(&1).unwrap().bucket;
        handle_request(&mut state, SearchRequest::Cancel { cid: 1 });
        assert!(state.pending.is_empty());
        assert!(state.buckets[bucket].is_empty());
    }

    #[test]
    fn poke_resets_attempts_and_engages_fast_mode() {
        let mut state = SearchEngineState::new();
        schedule_initial(&mut state, 1, "PV:1");
        // Simulate one prior attempt.
        if let Some(p) = state.pending.get_mut(&1) {
            p.attempt = 3;
        }
        state.poke();
        let p = state.pending.get(&1).unwrap();
        assert_eq!(p.attempt, 0, "poke must reset per-channel retry counter");
        assert_eq!(state.fast_ticks_remaining, N_SEARCH_BUCKETS as u32);
    }

    #[test]
    fn beacon_anomaly_for_pending_channel_keeps_bucket() {
        // pvxs poke() semantic: a BeaconAnomaly Schedule for an
        // already-pending channel must NOT move it to a new bucket.
        // Otherwise a mass-anomaly piles every pending search into
        // bucket=current+1 and defeats bucket distribution.
        let mut state = SearchEngineState::new();
        // Use Reconnect so it's placed into a non-current+1 bucket.
        handle_request(
            &mut state,
            SearchRequest::Schedule {
                cid: 7,
                pv_name: "PV:7".into(),
                reason: SearchReason::Reconnect,
            },
        );
        let original_bucket = state.pending.get(&7).unwrap().bucket;
        // Pretend prior attempts happened.
        if let Some(p) = state.pending.get_mut(&7) {
            p.attempt = 4;
        }
        // Now apply a BeaconAnomaly poke for cid=7.
        handle_request(
            &mut state,
            SearchRequest::Schedule {
                cid: 7,
                pv_name: "PV:7".into(),
                reason: SearchReason::BeaconAnomaly,
            },
        );
        let p = state.pending.get(&7).unwrap();
        assert_eq!(p.bucket, original_bucket, "poke must not relocate bucket");
        assert_eq!(p.attempt, 0);
        assert_eq!(state.fast_ticks_remaining, N_SEARCH_BUCKETS as u32);
        // And the bucket vector still has the cid exactly once.
        let count = state.buckets[original_bucket]
            .iter()
            .filter(|x| **x == 7)
            .count();
        assert_eq!(count, 1);
    }

    #[test]
    fn beacon_anomaly_schedule_pokes_engine() {
        let mut state = SearchEngineState::new();
        schedule_initial(&mut state, 1, "PV:1");
        // Pretend channel #1 had multiple prior failures.
        if let Some(p) = state.pending.get_mut(&1) {
            p.attempt = 2;
        }
        handle_request(
            &mut state,
            SearchRequest::Schedule {
                cid: 2,
                pv_name: "PV:2".into(),
                reason: SearchReason::BeaconAnomaly,
            },
        );
        // Both channels should now be at attempt=0 and the engine in fast mode.
        assert_eq!(state.pending.get(&1).unwrap().attempt, 0);
        assert_eq!(state.pending.get(&2).unwrap().attempt, 0);
        assert_eq!(state.fast_ticks_remaining, N_SEARCH_BUCKETS as u32);
    }

    #[test]
    fn connect_success_clears_pending_and_penalty() {
        let mut state = SearchEngineState::new();
        let server: SocketAddr = "127.0.0.1:5064".parse().unwrap();
        schedule_initial(&mut state, 1, "PV:1");
        state.penalty.insert(
            server,
            PenaltyEntry {
                until: Instant::now() + Duration::from_secs(60),
            },
        );
        handle_request(
            &mut state,
            SearchRequest::ConnectResult {
                cid: 1,
                success: true,
                server_addr: server,
            },
        );
        assert!(state.pending.is_empty());
        assert!(!state.penalty.contains_key(&server));
    }

    #[test]
    fn connect_failure_inserts_penalty() {
        let mut state = SearchEngineState::new();
        let server: SocketAddr = "127.0.0.1:5064".parse().unwrap();
        schedule_initial(&mut state, 1, "PV:1");
        handle_request(
            &mut state,
            SearchRequest::ConnectResult {
                cid: 1,
                success: false,
                server_addr: server,
            },
        );
        // Pending entry stays — channel still searching for another server.
        assert!(state.pending.contains_key(&1));
        assert!(state.penalty.contains_key(&server));
    }

    #[test]
    fn n_search_buckets_is_30() {
        // Sanity: pvxs uses 30, our bucket vector must match.
        let state = SearchEngineState::new();
        assert_eq!(state.buckets.len(), N_SEARCH_BUCKETS);
        assert_eq!(N_SEARCH_BUCKETS, 30);
    }

    #[test]
    fn fast_tick_revolution_covers_full_ring() {
        // FAST_TICK * N_SEARCH_BUCKETS should be ~6 s (matches pvxs poke cadence).
        let revolution = FAST_TICK * N_SEARCH_BUCKETS as u32;
        assert!(revolution >= Duration::from_secs(5));
        assert!(revolution <= Duration::from_secs(7));
    }

    /// `Initial` is the only reason that earns the immediate-fire
    /// `Some(cid)` return — `Reconnect` and `BeaconAnomaly` must
    /// return `None` so the main loop's `try_recv` drain doesn't
    /// batch a 5000-channel disconnect cascade into a single-tick
    /// burst (review finding HIGH#1).
    #[test]
    fn reconnect_and_beacon_anomaly_skip_immediate_fire() {
        let mut state = SearchEngineState::new();
        // Initial → Some(cid)
        let cid_initial = handle_request(
            &mut state,
            SearchRequest::Schedule {
                cid: 100,
                pv_name: "PV:Initial".into(),
                reason: SearchReason::Initial,
            },
        );
        assert_eq!(
            cid_initial,
            Some(100),
            "Initial must return Some for immediate fire"
        );
        // Reconnect → None (bucket-spread, no burst)
        let cid_reconnect = handle_request(
            &mut state,
            SearchRequest::Schedule {
                cid: 101,
                pv_name: "PV:Reconnect".into(),
                reason: SearchReason::Reconnect,
            },
        );
        assert_eq!(cid_reconnect, None, "Reconnect must NOT immediately fire");
        // BeaconAnomaly (NEW cid) → None (fast-tick handles retransmit)
        let cid_anomaly = handle_request(
            &mut state,
            SearchRequest::Schedule {
                cid: 102,
                pv_name: "PV:Anomaly".into(),
                reason: SearchReason::BeaconAnomaly,
            },
        );
        assert_eq!(
            cid_anomaly, None,
            "BeaconAnomaly NEW must NOT immediately fire"
        );
    }

    /// pvxs `Channel::disconnect` parity: `Reconnect` schedules
    /// must land in `current_bucket` (zero holdoff for the typical
    /// Active disconnect — `client.cpp:213`). Cascade-spread on
    /// first reconnect is achieved by the natural one-bucket-per-
    /// tick rate-limit, not by per-cid hashing. The earlier
    /// `(current+1+cid%30)` formula gave 1-30 s reconnect latency
    /// that the channel layer's wait-for-Found path couldn't hide.
    #[test]
    fn placement_reconnect_uses_current_bucket() {
        for current in 0..N_SEARCH_BUCKETS {
            assert_eq!(
                placement_bucket(current, SearchReason::Reconnect),
                current,
                "Reconnect must drop in current bucket (got {current})"
            );
        }
    }

    /// `Initial` and `BeaconAnomaly` both pair with an immediate
    /// broadcast / fast-tick retransmit, so their bucket placement
    /// is one tick ahead — that's where the FIRST scheduled
    /// retransmit (after the immediate fire) lands. Wrap-around at
    /// the ring boundary is part of the contract.
    #[test]
    fn placement_initial_and_beacon_anomaly_one_bucket_ahead() {
        for reason in [SearchReason::Initial, SearchReason::BeaconAnomaly] {
            assert_eq!(placement_bucket(0, reason), 1);
            assert_eq!(placement_bucket(13, reason), 14);
            assert_eq!(
                placement_bucket(N_SEARCH_BUCKETS - 1, reason),
                0,
                "wrap-around at ring boundary"
            );
        }
    }

    /// pvxs `tickSearch` line 1193-1196 escalates the retry bucket
    /// by `nSearch+1` after each transmit. Pattern: 1, 2, 3, ...,
    /// capping at `N_SEARCH_BUCKETS` (where the cap means "full
    /// ring", which lands back on the same bucket → 30 s
    /// steady-state retry cadence).
    #[test]
    fn cascade_next_implements_pvxs_nsearch_escalation() {
        let no_imbalance = |_| 0usize;
        let current = 7;

        assert_eq!(
            cascade_smoothed_next(current, 1, no_imbalance),
            (current + 1) % N_SEARCH_BUCKETS,
        );
        assert_eq!(
            cascade_smoothed_next(current, 2, no_imbalance),
            (current + 2) % N_SEARCH_BUCKETS,
        );
        assert_eq!(
            cascade_smoothed_next(current, 10, no_imbalance),
            (current + 10) % N_SEARCH_BUCKETS,
        );
        assert_eq!(
            cascade_smoothed_next(current, N_SEARCH_BUCKETS as u32, no_imbalance),
            current,
            "attempt at cap wraps to current (full-ring steady state)",
        );
        assert_eq!(
            cascade_smoothed_next(current, 1_000_000, no_imbalance),
            current,
            "attempt > cap stays clamped",
        );
    }

    /// pvxs `client.cpp:1199-1206` smoothing: when the chosen
    /// `next` bucket is overloaded versus `next+1` by 100+ entries,
    /// defer to `next+1`. Crosses two ticks instead of one.
    #[test]
    fn cascade_smoothing_defers_when_next_is_overloaded() {
        let current = 5;
        let attempt = 1; // → next=6, nextnext=7

        let overloaded = |idx: usize| if idx == 6 { 200 } else { 0 };
        assert_eq!(
            cascade_smoothed_next(current, attempt, overloaded),
            7,
            "delta > 100 must defer"
        );

        let below = |idx: usize| if idx == 6 { 90 } else { 0 };
        assert_eq!(
            cascade_smoothed_next(current, attempt, below),
            6,
            "delta < 100 stays in next"
        );

        let balanced = |idx: usize| if idx == 6 || idx == 7 { 200 } else { 0 };
        assert_eq!(cascade_smoothed_next(current, attempt, balanced), 6);

        let reverse = |idx: usize| if idx == 7 { 200 } else { 0 };
        assert_eq!(
            cascade_smoothed_next(current, attempt, reverse),
            6,
            "smoothing only defers forward, never backward"
        );
    }

    /// Smoothing boundary cases — pvxs's threshold is strictly
    /// `delta > 100`. Catches the easy-to-introduce off-by-one
    /// (`>= 100`).
    #[test]
    fn cascade_smoothing_boundary_at_delta_100() {
        let current = 5;
        let attempt = 1;
        let exactly_100 = |idx: usize| if idx == 6 { 100 } else { 0 };
        assert_eq!(
            cascade_smoothed_next(current, attempt, exactly_100),
            6,
            "delta == 100 must NOT trigger"
        );
        let just_over_100 = |idx: usize| if idx == 6 { 101 } else { 0 };
        assert_eq!(
            cascade_smoothed_next(current, attempt, just_over_100),
            7,
            "delta == 101 must trigger"
        );
    }

    /// Issue #372 mass-channel scenario, single-tick view: simulate
    /// the rearm half of one `process_bucket` call against a
    /// 5000-channel reconnect storm and verify the inline-push
    /// `cascade_smoothed_next` placement at least bisects the load
    /// instead of piling every channel into a single bucket.
    ///
    /// pvxs's smoothing rule (`client.cpp:1199-1206`) defers ONLY by
    /// one bucket (`next` → `nextnext`) when the chosen bucket
    /// exceeds `nextnext + 100`, so within one tick a flat-attempt
    /// reconnect storm can land in at most two buckets. The
    /// follow-on test
    /// `mass_5000_multi_tick_distribution_covers_full_ring`
    /// pins the ring-wide spread that emerges across multiple ticks.
    #[test]
    fn mass_5000_reconnect_spreads_at_least_two_buckets() {
        const N_CHANNELS: usize = 5000;
        let current = 0;
        let attempt = 1; // Reconnect → first retry uses attempt=1

        let mut buckets = vec![0usize; N_SEARCH_BUCKETS];
        for _sid in 0..N_CHANNELS {
            let bucket_sizes = |idx: usize| buckets[idx];
            let next = cascade_smoothed_next(current, attempt, bucket_sizes);
            buckets[next] += 1;
        }

        let total: usize = buckets.iter().sum();
        assert_eq!(
            total, N_CHANNELS,
            "every channel must be placed exactly once"
        );

        let nonempty = buckets.iter().filter(|&&n| n > 0).count();
        assert!(
            nonempty >= 2,
            "smoothing must split the load across ≥2 buckets; got {} non-empty: {buckets:?}",
            nonempty
        );

        // No single bucket may carry more than 60% of the total —
        // a regressed smoothing threshold would let bucket 1 take
        // all 5000 entries.
        let max_load = *buckets.iter().max().unwrap();
        let cap = (N_CHANNELS * 60) / 100;
        assert!(
            max_load <= cap,
            "no single bucket may carry > {cap} entries (60% of {N_CHANNELS}); \
             got max {max_load} in {buckets:?}"
        );
    }

    /// Issue #372 multi-tick scenario: simulate `process_bucket`
    /// running for `2 * N_SEARCH_BUCKETS` ticks against an initial
    /// bulk reconnect of 5000 channels, advancing `current_bucket`
    /// each tick and rearming sids via the inline-push smoothing.
    /// Verify that across the full ring rotation the load distributes
    /// across the majority of buckets and no bucket dominates more
    /// than a fraction of the total — proving the per-tick send rate
    /// stays bounded under sustained mass-channel load.
    #[test]
    fn mass_5000_multi_tick_distribution_covers_full_ring() {
        const N_CHANNELS: usize = 5000;
        const TICKS: usize = 2 * N_SEARCH_BUCKETS;

        // Initial state: all sids placed in bucket 0 with attempt=0
        // (mirrors a fresh Reconnect storm at process_bucket entry).
        let mut buckets: Vec<Vec<u32>> = (0..N_SEARCH_BUCKETS).map(|_| Vec::new()).collect();
        buckets[0] = (0..N_CHANNELS as u32).collect();
        let mut attempts = vec![0u32; N_CHANNELS];

        // Track maximum bucket load observed at the moment of
        // processing — that is the per-tick send rate ceiling.
        let mut max_per_tick = 0usize;
        let mut buckets_visited = [false; N_SEARCH_BUCKETS];

        let mut current = 0;
        for _ in 0..TICKS {
            buckets_visited[current] = true;
            let processing = std::mem::take(&mut buckets[current]);
            max_per_tick = max_per_tick.max(processing.len());

            // Rearm each sid via inline-push smoothing.
            for sid in processing {
                attempts[sid as usize] = attempts[sid as usize].saturating_add(1);
                let attempt = attempts[sid as usize];
                let bucket_sizes = |idx: usize| buckets[idx].len();
                let next = cascade_smoothed_next(current, attempt, bucket_sizes);
                buckets[next].push(sid);
            }

            current = (current + 1) % N_SEARCH_BUCKETS;
        }

        // Across one full ring + extra slack, every bucket should
        // have been visited as `current` rotates.
        let visited_count = buckets_visited.iter().filter(|&&v| v).count();
        assert_eq!(
            visited_count, N_SEARCH_BUCKETS,
            "current_bucket must rotate through every slot in {TICKS} ticks; got {visited_count}"
        );

        // The first tick processes the entire 5000-bulk; subsequent
        // ticks see the smoothed redistribution. Cap is the initial
        // bulk size — anything over that means the smoothing
        // accumulated load *back* into a single bucket faster than
        // the ring could drain it (regression).
        assert!(
            max_per_tick <= N_CHANNELS,
            "per-tick processing load must not exceed initial burst {N_CHANNELS}; got {max_per_tick}"
        );

        // Conservation: every sid still accounted for somewhere.
        let still_pending: usize = buckets.iter().map(|b| b.len()).sum();
        assert_eq!(
            still_pending, N_CHANNELS,
            "sids must not be lost across {TICKS} ticks; got {still_pending} pending of {N_CHANNELS}"
        );
    }

    /// End-to-end Reconnect bucket-fire test. Boots `run_search_engine`
    /// with a sniffer socket as the only addr_list destination,
    /// submits a `Schedule { Reconnect }`, and asserts that a
    /// SEARCH packet for the right cid lands on the sniffer within
    /// ~1.1 s — i.e. the next tick after Schedule arrival, mirroring
    /// pvxs `Channel::disconnect` recovery timing. Without the
    /// pvxs-parity placement the search would have been placed in a
    /// cid-hashed bucket up to 30 s away and never fired within a
    /// reasonable window.
    #[tokio::test(flavor = "current_thread")]
    async fn reconnect_search_broadcasts_within_one_tick() {
        use std::net::Ipv4Addr;

        // Sniffer on loopback ephemeral. Used as the engine's
        // ONLY addr_list destination.
        let sniffer = AsyncUdpV4::bind_single(Ipv4Addr::LOCALHOST, 0, false).expect("bind sniffer");
        let sniffer_addr = sniffer
            .local_addrs()
            .first()
            .copied()
            .expect("sniffer local_addr");

        let (req_tx, req_rx) = mpsc::unbounded_channel();
        let (resp_tx, _resp_rx) = mpsc::unbounded_channel();
        let engine_handle = tokio::spawn(run_search_engine(
            vec![crate::client::AddrEntry::new(
                sniffer_addr,
                None,
                sniffer_addr.port(),
            )],
            Vec::new(),
            req_rx,
            resp_tx,
            std::sync::Arc::new(dashmap::DashMap::new()),
        ));

        // Schedule a Reconnect for cid=42. Engine places it in
        // current_bucket; the next 1-Hz tick fires the broadcast.
        let cid = 42u32;
        let pv = "TEST:CA:RECONNECT:PV";
        let started = std::time::Instant::now();
        req_tx
            .send(SearchRequest::Schedule {
                cid,
                pv_name: pv.into(),
                reason: SearchReason::Reconnect,
            })
            .expect("schedule send");

        let mut buf = vec![0u8; 4096];
        let recv_result = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let (n, _from) = sniffer.recv_from(&mut buf).await?;
                if buf[..n].windows(pv.len()).any(|w| w == pv.as_bytes()) {
                    return Ok::<usize, std::io::Error>(n);
                }
            }
        })
        .await;

        let elapsed = started.elapsed();
        engine_handle.abort();

        let n = recv_result
            .expect("Reconnect SEARCH must arrive within 3 s")
            .expect("recv_from must not error");
        assert!(
            n > 0,
            "received an empty datagram — Reconnect SEARCH path is broken"
        );
        // Tight assertion catches the regression we're guarding
        // against (cid-hashed 1-30 s pre-fix latency) without being
        // flaky on a loaded CI runner. 2.5 s gives ~1.5 s slack on
        // top of the ≤ 1.1 s pvxs-parity target.
        assert!(
            elapsed < Duration::from_millis(2500),
            "Reconnect should broadcast within ~1.1 s (one tick); \
             took {elapsed:?} — bucket placement / tick handler may \
             have regressed"
        );
    }

    /// End-to-end retry escalation timing test. Verifies that the
    /// production process_bucket loop reproduces pvxs's `nSearch+1`
    /// pattern at the actual scheduler level — unit tests of
    /// `cascade_smoothed_next` cover the formula in isolation, but
    /// only this test catches an accumulator drift between the
    /// pure fn and the live `current_bucket`-advancing tick loop.
    ///
    /// Expected SEARCH arrival times (relative to Schedule submission):
    ///   #1 at ~1 s   (first tick after Schedule lands)
    ///   #2 at ~2 s   (idx+1, +1 cycle)
    ///   #3 at ~4 s   (idx+(1+2)=idx+3, +2 cycles)
    ///
    /// Slack: ±500 ms per gap to absorb scheduler / mio jitter on
    /// loaded CI. Total runtime ~4 s.
    #[tokio::test(flavor = "current_thread")]
    async fn retry_escalation_pvxs_pattern() {
        use std::net::Ipv4Addr;

        let sniffer = AsyncUdpV4::bind_single(Ipv4Addr::LOCALHOST, 0, false).expect("bind sniffer");
        let sniffer_addr = sniffer
            .local_addrs()
            .first()
            .copied()
            .expect("sniffer addr");

        let (req_tx, req_rx) = mpsc::unbounded_channel();
        let (resp_tx, _resp_rx) = mpsc::unbounded_channel();
        let engine_handle = tokio::spawn(run_search_engine(
            vec![crate::client::AddrEntry::new(
                sniffer_addr,
                None,
                sniffer_addr.port(),
            )],
            Vec::new(),
            req_rx,
            resp_tx,
            std::sync::Arc::new(dashmap::DashMap::new()),
        ));

        let cid = 77u32;
        let pv = "ESCALATION:CA";
        let started = std::time::Instant::now();
        req_tx
            .send(SearchRequest::Schedule {
                cid,
                pv_name: pv.into(),
                reason: SearchReason::Reconnect,
            })
            .expect("schedule");

        let mut buf = vec![0u8; 4096];
        let mut packet_times = Vec::new();
        for i in 0..3 {
            let t = tokio::time::timeout(Duration::from_secs(8), async {
                loop {
                    let (n, _) = sniffer.recv_from(&mut buf).await.expect("recv");
                    if buf[..n].windows(pv.len()).any(|w| w == pv.as_bytes()) {
                        return started.elapsed();
                    }
                }
            })
            .await
            .unwrap_or_else(|_| panic!("SEARCH #{} did not arrive within 8 s", i + 1));
            packet_times.push(t);
        }

        engine_handle.abort();

        assert!(
            packet_times[0] < Duration::from_millis(1500),
            "first SEARCH should arrive ~1 s after Schedule; got {:?}",
            packet_times[0]
        );
        let gap_12 = packet_times[1].saturating_sub(packet_times[0]);
        let gap_23 = packet_times[2].saturating_sub(packet_times[1]);
        assert!(
            (700..=1500).contains(&(gap_12.as_millis() as u64)),
            "gap #1→#2 should be ~1 s (nSearch=1); got {gap_12:?}. \
             Production retry escalation may have regressed."
        );
        assert!(
            (1500..=2700).contains(&(gap_23.as_millis() as u64)),
            "gap #2→#3 should be ~2 s (nSearch=2); got {gap_23:?}. \
             Production retry escalation may have regressed."
        );
    }
}
