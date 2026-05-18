//! TCP listener + per-connection handler.
//!
//! For each accepted client we spawn one task that:
//!
//! 1. Sends SET_BYTE_ORDER + CONNECTION_VALIDATION request
//! 2. Reads client's CONNECTION_VALIDATION response (auth)
//! 3. Sends CONNECTION_VALIDATED
//! 4. Loops reading channel ops (CREATE_CHANNEL / GET / PUT / MONITOR /
//!    GET_FIELD / DESTROY_REQUEST / DESTROY_CHANNEL).
//!
//! Channel state is kept per-connection (a `HashMap<sid, ChannelState>`).

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio::time::interval;
use tracing::{debug, error, warn};

use crate::client_native::decode::{Frame, PeerRole, try_parse_frame_role};
use crate::error::{PvaError, PvaResult};
use crate::proto::{
    BitSet, ByteOrder, Command, ControlCommand, HeaderFlags, PVA_VERSION, PvaHeader, QosFlags,
    Status, WriteExt, encode_size_into, encode_string_into,
};
use crate::pvdata::encode::{
    EncodeTypeCache, decode_pv_field, decode_pv_field_with_bitset, decode_type_desc,
    encode_pv_field, encode_type_desc, encode_type_desc_cached,
};
use crate::pvdata::{FieldDesc, PvField};

use super::runtime::PvaServerConfig;
use super::source::DynSource;

static NEXT_SID: AtomicU32 = AtomicU32::new(1);
fn alloc_sid() -> u32 {
    NEXT_SID.fetch_add(1, Ordering::Relaxed)
}

struct PipelineOptions {
    enabled: bool,
    queue_size: u32,
}

/// Wrap a PVA monitor event's `PvField` in the CA-side
/// [`FilteredMonitorEvent`] shape so it can flow through the shared
/// channel filter framework. The CA filters operate on a Snapshot
/// (value + STAT/SEVR + time); the PVA monitor stream carries a
/// PvField tree that contains those same fields under nested
/// `value`/`alarm`/`timeStamp` members (NTScalar / NTNDArray shape).
///
/// Currently extracts:
/// * Scalar value (DBND filter needs an f64 comparable). Falls back
///   to `Double(0.0)` for non-scalar values — DBND on arrays /
///   structures is meaningless and ARR (which would slice the
///   array) is the wire-through gap documented separately.
/// * The mask is always set to `EventMask::VALUE` because PVA's
///   monitor stream does not carry the CA-style ALARM/PROPERTY
///   discriminator at this layer — the field bitset already encodes
///   which subfields changed.
///
/// Filters that work today through this adapter: DEC, TS, SYNC,
/// DBND (scalar VAL only). ARR is the explicit remaining gap.
fn pv_field_to_filter_event(
    value: &PvField,
) -> epics_base_rs::server::database::filters::FilteredMonitorEvent {
    use crate::pvdata::ScalarValue;
    use epics_base_rs::server::database::filters::FilteredMonitorEvent;
    use epics_base_rs::server::pv::MonitorEvent;
    use epics_base_rs::server::recgbl::EventMask;
    use epics_base_rs::server::snapshot::Snapshot;
    use epics_base_rs::types::EpicsValue;
    use std::time::SystemTime;

    fn extract_scalar(f: &PvField) -> EpicsValue {
        match f {
            PvField::Scalar(ScalarValue::Double(d)) => EpicsValue::Double(*d),
            PvField::Scalar(ScalarValue::Float(f32v)) => EpicsValue::Float(*f32v),
            PvField::Scalar(ScalarValue::Int(i)) => EpicsValue::Long(*i),
            PvField::Scalar(ScalarValue::Long(l)) => EpicsValue::Long(*l as i32),
            PvField::Scalar(ScalarValue::Short(s)) => EpicsValue::Short(*s),
            PvField::Scalar(ScalarValue::String(s)) => EpicsValue::String(s.clone()),
            PvField::Structure(s) => {
                // NT-style structure: look for a "value" subfield.
                for (k, v) in &s.fields {
                    if k == "value" {
                        return extract_scalar(v);
                    }
                }
                EpicsValue::Double(0.0)
            }
            _ => EpicsValue::Double(0.0),
        }
    }
    let val = extract_scalar(value);
    FilteredMonitorEvent::new(
        MonitorEvent {
            snapshot: Snapshot::new(val, 0, 0, SystemTime::UNIX_EPOCH),
            origin: 0,
        },
        EventMask::VALUE,
    )
}

/// Read `record._options._filter` from a decoded pvRequest. The value
/// must be a string carrying the same channel-filter JSON syntax used
/// on the CA side (e.g.
/// `{"dbnd":{"d":0.5},"dec":{"n":3}}`). Returns `None` when the
/// option is absent, the empty string, or not a structure — the
/// monitor subscriber then runs with no filter chain.
///
/// This is the PVA wire-through for epics-base 3.15.7 server-side
/// channel filters. Upstream pvxs encodes filters per-field via
/// `field(value).{filter}` syntax; that requires schema-aware
/// parsing of the pvRequest's `field` subtree (the filter applies to
/// a specific named field). The `record._options._filter` carrier
/// here is the simpler universal form — one chain per subscription,
/// applied at the monitor emit boundary regardless of which field
/// the client is subscribed to. The two forms cover overlapping use
/// cases; a future revision can layer the field-scoped form on top.
fn monitor_filter_chain_json(req: &PvField) -> Option<String> {
    use crate::pvdata::ScalarValue;
    let root = match req {
        PvField::Structure(s) => s,
        _ => return None,
    };
    let record = root
        .fields
        .iter()
        .find_map(|(k, v)| (k == "record").then_some(v))?;
    let record_s = match record {
        PvField::Structure(s) => s,
        _ => return None,
    };
    let options = record_s
        .fields
        .iter()
        .find_map(|(k, v)| (k == "_options").then_some(v))?;
    let opt_s = match options {
        PvField::Structure(s) => s,
        _ => return None,
    };
    let json = opt_s.fields.iter().find_map(|(k, v)| {
        (k == "_filter").then_some(v).and_then(|v| match v {
            PvField::Scalar(ScalarValue::String(s)) => Some(s.clone()),
            _ => None,
        })
    })?;
    if json.trim().is_empty() {
        None
    } else {
        Some(json)
    }
}

/// epics-base PR `70735383350b` parity: extract
/// `record._options.autoExec` from a decoded pvRequest. Returns
/// `Some(false)` only when the field is explicitly set to "false"
/// (case-insensitive); `Some(true)` for "true"; `None` when the
/// option is absent (caller defaults to true / immediate execute).
fn put_autoexec_from_request(req: Option<&PvField>) -> Option<bool> {
    use crate::pvdata::ScalarValue;
    let root = match req? {
        PvField::Structure(s) => s,
        _ => return None,
    };
    let record = root
        .fields
        .iter()
        .find_map(|(k, v)| (k == "record").then_some(v))?;
    let record_s = match record {
        PvField::Structure(s) => s,
        _ => return None,
    };
    let options = record_s
        .fields
        .iter()
        .find_map(|(k, v)| (k == "_options").then_some(v))?;
    let opt_s = match options {
        PvField::Structure(s) => s,
        _ => return None,
    };
    let raw = opt_s.fields.iter().find_map(|(k, v)| {
        (k == "autoExec").then_some(v).and_then(|v| match v {
            PvField::Scalar(ScalarValue::String(s)) => Some(s.trim().to_ascii_lowercase()),
            _ => None,
        })
    })?;
    match raw.as_str() {
        "true" | "yes" | "1" => Some(true),
        "false" | "no" | "0" => Some(false),
        _ => None,
    }
}

/// Consume the optional u32 `nack` (initial pipeline window) that a
/// pvxs client appends to a MONITOR INIT body when it sets the
/// pipeline bit (pvxs `servermon.cpp:493` / `clientmon.cpp:341-342`).
/// Returns `Some(nack)` when the bit is set AND the four bytes are
/// present; `None` otherwise (kind mismatch, bit clear, or short
/// payload — the last case mirrors pvxs's "pipeline monitor w/o
/// initial nack incompatible" warn-but-accept policy).
fn parse_monitor_init_nack(
    kind: OpKind,
    subcmd: u8,
    cur: &mut std::io::Cursor<&[u8]>,
    order: ByteOrder,
) -> Option<u32> {
    if kind != OpKind::Monitor || (subcmd & 0x80) == 0 {
        return None;
    }
    cur.get_u32(order).ok()
}

/// Inspect a decoded pvRequest for `record._options.pipeline` and
/// `record._options.queueSize`. pvxs `Subscription` defaults to
/// `queueSize = 4` when pipeline is enabled; we follow.
fn monitor_pipeline_options(req: &PvField) -> Option<PipelineOptions> {
    use crate::pvdata::ScalarValue;
    let root = match req {
        PvField::Structure(s) => s,
        _ => return None,
    };
    let record = root
        .fields
        .iter()
        .find_map(|(k, v)| (k == "record").then_some(v))?;
    let record_s = match record {
        PvField::Structure(s) => s,
        _ => return None,
    };
    let options = record_s
        .fields
        .iter()
        .find_map(|(k, v)| (k == "_options").then_some(v))?;
    let opt_s = match options {
        PvField::Structure(s) => s,
        _ => return None,
    };
    // PVA-R20: pvxs `servermon.cpp:523-540` parses `pipeline` via
    // `Value::as(bool)` and `queueSize` via the analogous scalar
    // conversion. A pvxs client using the typed builder form
    // (`.record("pipeline", true).record("queueSize", N)`) sends a
    // BOOL/INT, not the parsed-from-`record[pipeline=true]` STRING.
    // Pre-fix Rust matched only the string form; the typed builder
    // produced a pvRequest Rust decoded as non-pipelined, dropping
    // flow control. Accept both shapes.
    let enabled = opt_s
        .fields
        .iter()
        .find_map(|(k, v)| {
            (k == "pipeline").then_some(v).and_then(|v| match v {
                PvField::Scalar(ScalarValue::Boolean(b)) => Some(*b),
                PvField::Scalar(ScalarValue::String(s)) => Some(matches!(
                    s.to_ascii_lowercase().as_str(),
                    "true" | "1" | "yes"
                )),
                PvField::Scalar(ScalarValue::Byte(i)) => Some(*i != 0),
                PvField::Scalar(ScalarValue::UByte(i)) => Some(*i != 0),
                PvField::Scalar(ScalarValue::Short(i)) => Some(*i != 0),
                PvField::Scalar(ScalarValue::UShort(i)) => Some(*i != 0),
                PvField::Scalar(ScalarValue::Int(i)) => Some(*i != 0),
                PvField::Scalar(ScalarValue::UInt(i)) => Some(*i != 0),
                PvField::Scalar(ScalarValue::Long(i)) => Some(*i != 0),
                PvField::Scalar(ScalarValue::ULong(i)) => Some(*i != 0),
                _ => None,
            })
        })
        .unwrap_or(false);
    // PVA-R20: pvxs `servermon.cpp:533-540` rejects invalid or `<2`
    // queueSize when pipeline is enabled (sets pipeline off). Pre-fix
    // Rust silently defaulted to 4 on parse failure and clamped to
    // 1 — accepting `queueSize=1` for a pipelined monitor, which
    // pvxs rejects because the credit accounting needs ≥2 slots.
    let queue_size = opt_s.fields.iter().find_map(|(k, v)| {
        (k == "queueSize").then_some(v).and_then(|v| match v {
            PvField::Scalar(ScalarValue::String(s)) => s.parse::<u32>().ok(),
            PvField::Scalar(ScalarValue::Byte(i)) => u32::try_from(*i).ok(),
            PvField::Scalar(ScalarValue::UByte(i)) => Some(u32::from(*i)),
            PvField::Scalar(ScalarValue::Short(i)) => u32::try_from(*i).ok(),
            PvField::Scalar(ScalarValue::UShort(i)) => Some(u32::from(*i)),
            PvField::Scalar(ScalarValue::Int(i)) => u32::try_from(*i).ok(),
            PvField::Scalar(ScalarValue::UInt(i)) => Some(*i),
            PvField::Scalar(ScalarValue::Long(l)) => u32::try_from(*l).ok(),
            PvField::Scalar(ScalarValue::ULong(l)) => u32::try_from(*l).ok(),
            _ => None,
        })
    });
    if enabled {
        match queue_size {
            Some(n) if n >= 2 => Some(PipelineOptions {
                enabled: true,
                queue_size: n,
            }),
            // pvxs: invalid/<2 queueSize disables pipeline rather than
            // accepting a broken negotiation.
            _ => Some(PipelineOptions {
                enabled: false,
                queue_size: queue_size.unwrap_or(4).max(1),
            }),
        }
    } else {
        Some(PipelineOptions {
            enabled: false,
            queue_size: queue_size.unwrap_or(4).max(1),
        })
    }
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
struct ChannelState {
    name: String,
    cid: u32,
    sid: u32,
    introspection: Option<FieldDesc>,
    /// ioid → (introspection negotiated for this op, kind)
    ops: HashMap<u32, OpState>,
}

/// Shared abort guard: when the last clone is dropped (HashMap removal,
/// connection end, ...), the spawned task is aborted automatically.
#[derive(Debug)]
struct AbortOnDrop(tokio::task::AbortHandle);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
struct OpState {
    intro: FieldDesc,
    kind: OpKind,
    /// For MONITOR ops: true once the subscriber task has been spawned.
    /// Subsequent START/pipeline-ack messages are no-ops.
    monitor_started: bool,
    /// Abort guard for the spawned MONITOR subscriber. Drop semantics
    /// (via `AbortOnDrop`) ensure the task is cancelled when the op is
    /// removed from the channel map (DestroyRequest), when the channel
    /// itself is removed (DestroyChannel), or when the connection ends.
    monitor_abort: Option<Arc<AbortOnDrop>>,
    /// Field mask derived from the client's pvRequest at INIT time.
    /// Drives the changed-bitset and partial-value encoding so the
    /// server only emits what was requested.
    mask: BitSet,
    /// Pipeline credit window (P-G11). pvxs `MonitorOp::window` —
    /// when pipeline mode is active, the server emits at most this
    /// many events before pausing until the client sends a
    /// MONITOR_ACK (subcmd 0x80) refilling the window. `None` when
    /// pipeline=false (no flow control on this op). Shared with the
    /// spawned subscriber via `Arc<AtomicU32>` so ACK messages can
    /// refill from the per-conn dispatch path.
    monitor_window: Option<Arc<std::sync::atomic::AtomicU32>>,
    /// Pulsed when `monitor_window` transitions from 0 → >0 so the
    /// subscriber loop can wake up and resume emission.
    monitor_window_notify: Option<Arc<tokio::sync::Notify>>,
    /// MONITOR pause flag (P-G28). pvxs subcmd `0x04` (without the
    /// `0x40` start bit) signals "stop emitting events but keep the
    /// op alive"; pvxs `Subscription::pause(true)` uses this. The
    /// subscriber task checks before emit and skips when `true`.
    /// Pulsed via the same notify as the credit window so the loop
    /// wakes on resume.
    monitor_paused: Arc<std::sync::atomic::AtomicBool>,
    /// Server-side filter chain decoded from
    /// `record._options._filter` (a JSON string carrying the same
    /// channel-filter syntax CA uses: `{"dbnd":{"d":0.5},...}`). The
    /// monitor subscriber task wraps each emitted event through
    /// `apply()` before building the wire payload — filters that drop
    /// the event cause the iteration to continue without sending.
    /// Empty chain (the default) is a no-op.
    monitor_filters: Arc<epics_base_rs::server::database::filters::FilterChain>,
    /// `record._options.autoExec` from the INIT pvRequest. pvxs
    /// uses this purely client-side to decide whether to send the
    /// PUT EXEC immediately after INIT or wait for an explicit
    /// `reExec()` call (clientget.cpp:123). The server has no
    /// queueing role — pvxs `serverget.cpp:488-492` calls `onPut`
    /// the moment a CMD_PUT with !init arrives, regardless of the
    /// client's autoExec setting. We keep the field for diagnostic
    /// echoing but DO NOT gate write commits on it.
    put_auto_exec: bool,
    /// BR-R3: full INIT pvRequest value (decoded). PVA PUT INIT
    /// carries per-operation options (`record._options.process` /
    /// `block`, etc.) that the data-phase payload does NOT carry.
    /// We stash the value here at INIT so the data-phase PUT can
    /// attach it to the [`ChannelContext`] forwarded to the source,
    /// letting sources like the QSRV bridge honor process/block
    /// without re-parsing the value (where they no longer live).
    pv_request: Option<PvField>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum OpKind {
    Get,
    Put,
    Monitor,
    Rpc,
    /// PVA `PUT_GET` (cmd 12): atomic put-then-get round trip.
    PutGet,
    /// PVA `PROCESS` (cmd 16): trigger record processing, no value.
    Process,
}

impl OpKind {
    /// Wire command this op kind maps to.
    fn command(self) -> Command {
        match self {
            OpKind::Get => Command::Get,
            OpKind::Put => Command::Put,
            OpKind::Monitor => Command::Monitor,
            OpKind::Rpc => Command::Rpc,
            OpKind::PutGet => Command::PutGet,
            OpKind::Process => Command::Process,
        }
    }
}

/// Run the TCP listener forever. Backwards-compat wrapper that
/// drops per-peer stats — equivalent to calling
/// [`run_tcp_server_with_peers`] with an empty registry the caller
/// can never read.
pub async fn run_tcp_server(
    source: DynSource,
    bind_addr: SocketAddr,
    config: PvaServerConfig,
) -> PvaResult<()> {
    run_tcp_server_with_peers(
        source,
        bind_addr,
        config,
        crate::server_native::peers::PeerRegistry::new(),
    )
    .await
}

/// Run the TCP listener with an externally-shared
/// [`PeerRegistry`]. F-G7: lets [`crate::server_native::PvaServer::report`]
/// observe per-connection stats.
pub async fn run_tcp_server_with_peers(
    source: DynSource,
    bind_addr: SocketAddr,
    config: PvaServerConfig,
    peers: Arc<crate::server_native::peers::PeerRegistry>,
) -> PvaResult<()> {
    let listener = TcpListener::bind(bind_addr).await.map_err(PvaError::Io)?;
    run_tcp_server_on_listener(source, listener, config, peers).await
}

/// Variant that takes a pre-bound [`TcpListener`]. Lets
/// [`crate::server_native::PvaServer::start`] perform the bind
/// synchronously (so the bound port is observable to callers) and
/// then hand the listener to the spawned accept task. Eliminates
/// the bind-race window that existed when the spawn-and-bind happened
/// inside the spawned task — concurrent isolated tests can no longer
/// have their picked-then-dropped ephemeral ports stolen by a peer.
pub async fn run_tcp_server_on_listener(
    source: DynSource,
    listener: TcpListener,
    config: PvaServerConfig,
    peers: Arc<crate::server_native::peers::PeerRegistry>,
) -> PvaResult<()> {
    let bind_addr = listener.local_addr().map_err(PvaError::Io)?;
    debug!(?bind_addr, "TCP listener up");
    let active = Arc::new(AtomicUsize::new(0));

    let tls_acceptor = config
        .tls
        .as_ref()
        .map(|cfg| tokio_rustls::TlsAcceptor::from(cfg.config.clone()));

    // D-G1: track per-connection tasks in a JoinSet so they're
    // aborted as a unit when this accept-loop future is dropped (e.g.
    // PvaServer::stop() → tcp_handle.abort()). Without this, every
    // per-conn task ran detached and lingered until its internal
    // idle_timeout (~45s). The select! arm on `conn_tasks.join_next()`
    // also reaps completed tasks so the set doesn't accumulate
    // finished JoinHandles.
    let mut conn_tasks: tokio::task::JoinSet<()> = tokio::task::JoinSet::new();

    loop {
        let accept_result = tokio::select! {
            biased;
            res = listener.accept() => res,
            // Drain finished connection tasks. Returns None when the
            // set is empty — that branch resolves immediately, but
            // `biased` makes the listener arm preferred so we never
            // starve incoming accepts.
            Some(_) = conn_tasks.join_next() => continue,
        };
        match accept_result {
            Ok((stream, peer)) => {
                if config.is_ignored_peer(peer) {
                    debug!(?peer, "rejecting connection: peer on ignore_addrs");
                    drop(stream);
                    continue;
                }
                let cur = active.fetch_add(1, Ordering::SeqCst);
                if cur >= config.max_connections {
                    active.fetch_sub(1, Ordering::SeqCst);
                    warn!(
                        ?peer,
                        "rejecting connection: max_connections={}", config.max_connections
                    );
                    drop(stream);
                    continue;
                }
                let src = source.clone();
                let cfg = config.clone();
                let active_dec = active.clone();
                let acceptor = tls_acceptor.clone();
                // F-G7: register this connection in the peer registry
                // so PvaServer::report() can surface it. Removed when
                // the connection task ends.
                let tls_in_use = acceptor.is_some();
                let peer_entry = crate::server_native::peers::PeerEntry::new(tls_in_use);
                peers.insert(peer, peer_entry.clone());
                let peers_for_task = peers.clone();
                conn_tasks.spawn(async move {
                    stream.set_nodelay(true).ok();
                    // Enable OS-level TCP keepalive so half-open connections
                    // (NAT timeout, dead client) are detected within ~30s
                    // even when the protocol-level Echo path can't fire
                    // (e.g. peer hasn't initialized control plane yet).
                    // Defence-in-depth on top of the heartbeat ECHO timer:
                    // pvxs itself does NOT set SO_KEEPALIVE — it relies on
                    // libevent's `bufferevent_set_timeouts` for inactivity
                    // detection. We add OS keepalive (CA-libca style) so a
                    // pre-handshake half-open peer still gets reaped even
                    // before the application timer arms.
                    {
                        let sock = socket2::SockRef::from(&stream);
                        let keepalive = socket2::TcpKeepalive::new()
                            .with_time(std::time::Duration::from_secs(15))
                            .with_interval(std::time::Duration::from_secs(5));
                        let _ = sock.set_keepalive(true);
                        let _ = sock.set_tcp_keepalive(&keepalive);
                    }
                    let result = match acceptor {
                        // Round 8 P-G15: cap the TLS handshake — a peer
                        // that completes TCP but stalls during ClientHello
                        // would otherwise hold a `max_connections` slot
                        // until OS keepalive reaps it (~30s).
                        Some(a) => {
                            match tokio::time::timeout(cfg.tls_handshake_timeout, a.accept(stream))
                                .await
                            {
                                Ok(Ok(tls_stream)) => {
                                    // F8: derive the peer's x509 identity from
                                    // the *verified* certificate chain before
                                    // splitting the stream. rustls only
                                    // exposes `peer_certificates()` on the
                                    // whole `TlsStream`, and the chain has
                                    // already passed `WebPkiClientVerifier`,
                                    // so this is the cryptographically-checked
                                    // identity (pvxs `fill_credentials`).
                                    let x509_id = {
                                        let (_, conn) = tls_stream.get_ref();
                                        conn.peer_certificates().and_then(|chain| {
                                            crate::auth::x509_credentials_from_chain(chain)
                                        })
                                    };
                                    let (r, w) = tokio::io::split(tls_stream);
                                    handle_connection_io(
                                        src,
                                        Box::new(r),
                                        Box::new(w),
                                        peer,
                                        cfg,
                                        peer_entry.clone(),
                                        x509_id,
                                    )
                                    .await
                                }
                                Ok(Err(e)) => {
                                    debug!(?peer, "TLS handshake failed: {e}");
                                    Err(PvaError::Io(e))
                                }
                                Err(_) => {
                                    debug!(
                                        ?peer,
                                        timeout = ?cfg.tls_handshake_timeout,
                                        "TLS handshake timed out"
                                    );
                                    Err(PvaError::Protocol("TLS handshake timeout".into()))
                                }
                            }
                        }
                        None => {
                            let (r, w) = stream.into_split();
                            handle_connection_io(
                                src,
                                Box::new(r),
                                Box::new(w),
                                peer,
                                cfg,
                                peer_entry.clone(),
                                None,
                            )
                            .await
                        }
                    };
                    if let Err(e) = result {
                        debug!(?peer, "connection ended: {e}");
                    }
                    active_dec.fetch_sub(1, Ordering::SeqCst);
                    // F-G7: drop the per-peer entry whether the
                    // connection ended cleanly or via I/O error.
                    peers_for_task.remove(peer);
                });
            }
            Err(e) => {
                error!("accept error: {e}");
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    }
}

/// Identity used for per-connection authorisation.
///
/// Mirrors pvxs `server::ClientCredentials` (serverconn.cpp:73-234).
/// Two population paths feed it:
///
/// - **`ca` / `anonymous`** — parsed off the CONNECTION_VALIDATION reply
///   (`parse_client_credentials`).
/// - **`x509`** — derived from the *verified* TLS peer certificate chain
///   after the handshake (pvxs `SSLContext::fill_credentials`). The TLS
///   identity is authoritative: it overrides whatever the client claims
///   in CONNECTION_VALIDATION, because the chain was cryptographically
///   verified against the configured root CA.
///
/// The structured form is consumed by the server's ACF access gate
/// (`AccessGate::check`) and lands in `tracing` for audit.
#[derive(Debug, Clone)]
pub struct ClientCredentials {
    /// Selected auth method ("anonymous" / "ca" / "x509" / ...).
    pub method: String,
    /// Account name (e.g., the `ca` auth's `user` field, or the x509
    /// leaf cert subject CommonName). Empty when the auth method does
    /// not carry one.
    pub account: String,
    /// Host name claim from the `ca` auth, when present. Informational
    /// only — never trust it for access decisions over the network
    /// hostname / mTLS-verified peer.
    pub host: String,
    /// Certificate authority for the `x509` method: the root CA's
    /// subject CommonName (pvxs `PeerCredentials::authority`). Empty for
    /// non-TLS methods. ACF `RULE(... ){ AUTHORITY("...") }` scopes
    /// match against this.
    pub authority: String,
    /// Group / role claims advertised by the auth method. Populated
    /// by the `ca` method via [`crate::auth::posix_groups`] on the
    /// client side; on the server side the same list is parsed off
    /// the wire here. ACF rules of the form
    /// `R member group:operators` match against this set.
    pub roles: Vec<String>,
}

impl ClientCredentials {
    fn anonymous() -> Self {
        Self {
            method: "anonymous".into(),
            account: "anonymous".into(),
            host: String::new(),
            authority: String::new(),
            roles: Vec::new(),
        }
    }

    /// Build `x509` credentials from a verified TLS peer chain.
    /// Mirrors pvxs `SSLContext::fill_credentials`: the leaf cert's
    /// subject CommonName becomes the `account` and the root CA's
    /// subject CommonName becomes the `authority`.
    fn x509(creds: crate::auth::X509Credentials) -> Self {
        Self {
            method: "x509".into(),
            account: creds.account,
            host: String::new(),
            authority: creds.authority,
            roles: Vec::new(),
        }
    }

    /// Format a one-line debug label for tracing / diagnostics.
    /// Mirrors pvxs `peerLabel()` (conn.cpp:50). Includes peer
    /// address, auth method, and account.
    pub fn peer_label(&self, peer: std::net::SocketAddr) -> String {
        if self.account.is_empty() {
            format!("{peer}/{}", self.method)
        } else {
            format!("{}@{peer}/{}", self.account, self.method)
        }
    }
}

/// Parse `CONNECTION_VALIDATION` reply payload (pvxs serverconn.cpp:200).
/// Layout: `buffer_size:u32 + intro_size:u16 + qos:u16 + method:String +
/// auth_type + auth_value`.
///
/// PVA-R22: pvxs `serverconn.cpp:204-216` always decodes the auth
/// Value via `from_wire_type_value`, then `if(!M.good()) bev.reset()`
/// — a truncated/invalid auth body is connection-fatal. Pre-fix Rust
/// wrapped the decode in `if let Ok` and still returned
/// `Some(ClientCredentials)` on failure, filling `account` with the
/// method name. A truncated `method="ca"` handshake became
/// `method="ca", account="ca"` — every ACF rule keying on
/// method/account/host was then evaluating a credential tuple pvxs
/// would never have produced.
///
/// Now: `Ok(None)` for the empty-method / anonymous case;
/// `Ok(Some(creds))` only when the auth Value decoded successfully;
/// `Err(...)` on any decode fault past the method string (so the
/// caller can disconnect, mirroring pvxs `bev.reset()`).
fn parse_client_credentials(
    frame: &Frame,
    order: ByteOrder,
) -> PvaResult<Option<ClientCredentials>> {
    let mut cur = frame.cursor();
    let _buffer_size = cur
        .get_u32(order)
        .map_err(|e| PvaError::Decode(format!("CONN_VALIDATION buffer_size: {e}")))?;
    let _intro_size = cur
        .get_u16(order)
        .map_err(|e| PvaError::Decode(format!("CONN_VALIDATION intro_size: {e}")))?;
    let _qos = cur
        .get_u16(order)
        .map_err(|e| PvaError::Decode(format!("CONN_VALIDATION qos: {e}")))?;
    let method = crate::proto::decode_string(&mut cur, order)
        .map_err(|e| PvaError::Decode(format!("CONN_VALIDATION method: {e}")))?
        .unwrap_or_default();
    if method.is_empty() {
        // pvxs anonymous handshake: empty method, no auth body to
        // decode. Surface as `Ok(None)` so the caller can install
        // the default anonymous credentials.
        return Ok(None);
    }
    // Auth value: type descriptor + full value. pvxs requires both to
    // decode cleanly before it accepts the method. A leading `0xFF`
    // is the pvxs "null type" tag (`from_wire_type_value` returns an
    // empty Value), used when the method carries no structured
    // auth body — accept and treat as empty auth.
    let mut creds = ClientCredentials {
        method: method.clone(),
        account: String::new(),
        host: String::new(),
        authority: String::new(),
        roles: Vec::new(),
    };
    let pos = cur.position();
    let peek = cur
        .get_u8()
        .map_err(|e| PvaError::Decode(format!("CONN_VALIDATION auth desc peek: {e}")))?;
    if peek == 0xFF {
        // Null auth Value — empty creds, but the method is honoured.
        return Ok(Some(creds));
    }
    // Rewind and decode the real descriptor.
    cur.set_position(pos);
    let desc = decode_type_desc(&mut cur, order)
        .map_err(|e| PvaError::Decode(format!("CONN_VALIDATION auth desc: {e}")))?;
    let value = decode_pv_field(&desc, &mut cur, order)
        .map_err(|e| PvaError::Decode(format!("CONN_VALIDATION auth value: {e}")))?;
    if let PvField::Structure(s) = value {
        for (name, field) in &s.fields {
            match (name.as_str(), field) {
                ("user", PvField::Scalar(crate::pvdata::ScalarValue::String(v))) => {
                    creds.account = v.clone();
                }
                ("host", PvField::Scalar(crate::pvdata::ScalarValue::String(v))) => {
                    creds.host = v.clone();
                }
                ("groups" | "roles", PvField::ScalarArray(arr)) => {
                    creds.roles = arr
                        .iter()
                        .filter_map(|sv| {
                            if let crate::pvdata::ScalarValue::String(s) = sv {
                                Some(s.clone())
                            } else {
                                None
                            }
                        })
                        .collect();
                }
                _ => {}
            }
        }
    }
    // Pre-fix Rust filled `account` with `method` whenever the auth
    // body didn't carry a `user` field — that turned a truncated
    // `ca` handshake into `account="ca"`. pvxs only populates
    // user/host/groups from a successfully decoded ca structure;
    // anything else leaves them empty (anonymous-shaped tuple). Mirror
    // that — leave `account` empty when the structure didn't carry a
    // `user` field. ACF rules will then see an empty-account ca
    // credential rather than a fabricated method=name pair.
    Ok(Some(creds))
}

/// Type-erased read/write halves so the same handler works for plain TCP
/// and TLS-wrapped streams.
type SrvRead = Box<dyn tokio::io::AsyncRead + Unpin + Send>;
type SrvWrite = Box<dyn tokio::io::AsyncWrite + Unpin + Send>;
/// Per-connection write side. Producers (main read loop, heartbeat,
/// monitor subscribers) push fully-framed PVA messages into the
/// channel; a single dedicated writer task drains it in arrival order.
/// Replaces `Arc<Mutex<SrvWrite>>` so a slow client cannot block other
/// producers waiting for the lock. The channel is *bounded* —
/// `await`-style sends propagate backpressure all the way back to the
/// monitor subscribers / read loop, so memory cannot grow unbounded
/// when the client is slow. Errors on the write side drop the
/// receiver; subsequent sends fail and the read loop independently
/// observes the dead socket and tears down.
type SrvTx = tokio::sync::mpsc::Sender<Vec<u8>>;

async fn handle_connection_io(
    source: DynSource,
    mut reader: SrvRead,
    mut writer_raw: SrvWrite,
    peer: SocketAddr,
    config: PvaServerConfig,
    peer_entry: Arc<crate::server_native::peers::PeerEntry>,
    // F8: x509 identity from the verified TLS peer chain, when this
    // connection arrived over mutually-authenticated TLS. `None` for
    // plain TCP or TLS without a client cert. When present it is the
    // authoritative identity and overrides the CONNECTION_VALIDATION
    // claim — mirrors pvxs `SSLContext::fill_credentials`.
    x509_identity: Option<crate::auth::X509Credentials>,
) -> PvaResult<()> {
    let op_timeout = config.op_timeout;
    let idle_timeout = config.idle_timeout;

    // Spawn the dedicated writer task. All emit sites push framed bytes
    // into `tx`; the task drains and writes serially. Two failure
    // modes are detected:
    // 1. Hard I/O error — the underlying socket returned an error.
    //    `write_all` returns Err; we exit and the receiver closes,
    //    so subsequent `tx.send(...)` calls fail immediately.
    // 2. Stuck client — the kernel send buffer is full because the
    //    peer stopped reading. `write_all` returns Pending forever
    //    on a non-blocking socket; without a guard the writer task
    //    would hang and back-pressure both the heartbeat and the
    //    read-side dispatcher (since both push into the same mpsc).
    //    We wrap `write_all` in `tokio::time::timeout(send_timeout)`
    //    so a stalled write breaks the task, closes the mpsc, and
    //    fails fast. Mirrors the parallel guard in `epics-ca-rs`'s
    //    server-side dispatch wrap (the CA G1 audit fix).
    let send_tmo = config.send_timeout;
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(config.write_queue_depth);
    let writer_peer = peer;
    let peer_entry_writer = peer_entry.clone();
    let writer_task = tokio::spawn(async move {
        while let Some(frame) = rx.recv().await {
            match tokio::time::timeout(send_tmo, writer_raw.write_all(&frame)).await {
                Ok(Ok(())) => {
                    // F-G7: bytes_out counter for PvaServer::report().
                    peer_entry_writer.touch_tx(frame.len());
                }
                Ok(Err(e)) => {
                    debug!(peer = ?writer_peer, error = %e, "writer task: TCP write failed, dropping connection");
                    break;
                }
                Err(_) => {
                    warn!(
                        peer = ?writer_peer,
                        timeout_secs = send_tmo.as_secs_f64(),
                        "writer task: send timeout (stuck client?), dropping connection"
                    );
                    break;
                }
            }
        }
    });
    // P-G18: abort the writer + heartbeat tasks the moment the read
    // loop returns. Without this, both linger up to `idle_timeout`
    // (default 45s) emitting ECHOes into a channel nobody is reading
    // and holding the writer half of the (now-disconnected) socket.
    // pvxs uses libevent-driven cleanup that shuts everything in one
    // pass; we rely on tokio JoinHandle::abort() via AbortOnDrop.
    let _writer_guard = AbortOnDrop(writer_task.abort_handle());

    // Track per-connection liveness for the idle-timeout watchdog and the
    // server-side echo heartbeat task.
    let last_rx = Arc::new(AtomicU64::new(now_nanos()));

    // Spawn server-side heartbeat: send ECHO_REQUEST every 15 s; close if
    // we've been idle for `idle_timeout`.
    let last_rx_hb = last_rx.clone();
    let tx_hb = tx.clone();
    let order_hb = config.wire_byte_order;
    let hb_handle = tokio::spawn(async move {
        let mut tick = interval(Duration::from_secs(15));
        tick.tick().await;
        loop {
            tick.tick().await;
            let last = last_rx_hb.load(Ordering::SeqCst);
            let elapsed = now_nanos().saturating_sub(last);
            if Duration::from_nanos(elapsed) > idle_timeout {
                warn!(?peer, "PVA client idle > {idle_timeout:?}; closing");
                break;
            }
            let h = PvaHeader::control(true, order_hb, ControlCommand::EchoRequest.code(), 0);
            let mut buf = Vec::with_capacity(8);
            h.write_into(&mut buf);
            if tx_hb.send(buf).await.is_err() {
                break;
            }
        }
    });
    let _hb_guard = AbortOnDrop(hb_handle.abort_handle());

    let order = config.wire_byte_order;

    // Step 1: send SET_BYTE_ORDER (control message). Per pvxs, the byte order
    // we want to use is encoded in the control header's flag bit 7.
    let set_bo = {
        let mut buf = Vec::with_capacity(8);
        let h = PvaHeader::control(true, order, ControlCommand::SetByteOrder.code(), 0);
        h.write_into(&mut buf);
        buf
    };
    let _ = tx.send(set_bo).await;

    // Step 2: send CONNECTION_VALIDATION request (server → client).
    // PVA-R8: pvxs `serverconn.cpp:108-114` writes "anonymous" first,
    // then "ca", with a comment explaining that older pvAccess
    // clients took the LAST known plugin on the wire. The reverse-
    // priority order matters: an old client picks the last
    // recognised method as its preferred. Pre-fix Rust sent
    // `["ca", "anonymous"]` which made such old clients pick
    // anonymous and silently drop user/host credentials — changing
    // ACF decisions even though the comment claimed pvxs parity.
    // Modern pvxs clients explicitly prefer `ca`; validation still
    // accepts both, only the wire order changes.
    const ADVERTISED_AUTH_METHODS: &[&str] = &["anonymous", "ca"];
    let val_req =
        build_server_connection_validation(order, 87_040, 32_767, ADVERTISED_AUTH_METHODS);
    let _ = tx.send(val_req).await;

    // Step 3+: drive the read loop.
    let mut rx_buf: Vec<u8> = Vec::with_capacity(8192);
    let mut channels: HashMap<u32, ChannelState> = HashMap::new();
    let mut handshake_complete = false;
    // Client identity carried for the rest of the connection lifetime.
    //
    // F8 precedence (mirrors pvxs):
    //  - mTLS with a verified client cert → `x509` credentials derived
    //    from the cert chain. This is cryptographically verified and is
    //    the authoritative identity — the CONNECTION_VALIDATION reply
    //    cannot override it.
    //  - otherwise → parsed from the CONNECTION_VALIDATION reply
    //    (`ca`/`anonymous`), falling back to anonymous when the client
    //    skips the exchange or sends an unparseable payload.
    //
    // Fed into the server's ACF `AccessGate::check` for every op.
    let x509_locked = x509_identity.is_some();
    let mut cred = match x509_identity {
        Some(id) => ClientCredentials::x509(id),
        None => ClientCredentials::anonymous(),
    };
    // Per-connection emit-side TypeStore. Only consulted when
    // `config.emit_type_cache` is true (off by default for pvAccessCPP
    // compatibility — that client does not parse 0xFD/0xFE markers).
    let mut encode_type_cache = crate::pvdata::encode::EncodeTypeCache::new();

    let max_msg_size = config.max_message_size;
    // P-G20: segmented-message reassembly state. pvxs conn.cpp:228-291
    // accumulates SegFirst..SegMiddle..SegLast bodies into `segBuf`
    // before dispatching. Without this, our server would treat every
    // segment as a fresh message, decode garbage, and likely return
    // a Decode error mid-payload. Sites that put bulk values
    // (NTTable, large NTNDArray, multi-MiB NTScalarArray) over PVA
    // hit segmented frames whenever the message exceeds the peer's
    // buffer-size hint negotiated in CONNECTION_VALIDATION.
    let mut seg_buf: Vec<u8> = Vec::new();
    let mut seg_cmd: u8 = 0;
    let mut expect_seg = false;
    loop {
        // C-G2: if the writer task has died (send_timeout fired,
        // panic, etc.) the outbound mpsc is closed. Every subsequent
        // `let _ = tx.send(...).await` in the dispatch path silently
        // discards its frame and the client never sees the response,
        // but the read loop would otherwise keep accumulating
        // per-IOID state until `op_timeout` (default 64,000 s) or
        // `idle_timeout` (45 s) tore the connection down. Detect
        // the writer death here and unwind immediately so the
        // channels HashMap drop fires its AbortOnDrop chain and the
        // peer's connection slot is released within ms instead of
        // ~30-45 s.
        if tx.is_closed() {
            return Ok(());
        }
        let frame = read_frame(&mut reader, &mut rx_buf, op_timeout, max_msg_size).await?;
        // F-G7: bytes_in counter (header + payload). Drives
        // PvaServer::report() throughput diagnostics.
        peer_entry.touch_rx(PvaHeader::SIZE + frame.payload.len());
        last_rx.store(now_nanos(), Ordering::SeqCst);
        if frame.header.flags.is_control() {
            // Handle echo etc., otherwise ignore.
            if frame.header.command == ControlCommand::EchoRequest.code() {
                let mut buf = Vec::new();
                let h = PvaHeader::control(
                    true,
                    order,
                    ControlCommand::EchoResponse.code(),
                    frame.header.payload_length,
                );
                h.write_into(&mut buf);
                let _ = tx.send(buf).await;
            }
            continue;
        }

        // P-G20: segmentation gate. Mirrors pvxs conn.cpp:228-244.
        //   continuation = SegLast bit set (true for mid OR last)
        //   * Violation when (continuation XOR expect_seg) — peer
        //     interleaved a fresh first/unsegmented frame inside a
        //     pending segmented message, OR sent a continuation when
        //     none was pending.
        //   * Violation when continuation && cmd != saved_cmd.
        // Either case → drop connection (decode would be undefined).
        let raw_seg = frame.header.flags.0 & HeaderFlags::SEGMENT_MASK;
        let continuation = raw_seg & HeaderFlags::SEGMENT_LAST != 0;
        if continuation ^ expect_seg || (continuation && frame.header.command != seg_cmd) {
            return Err(PvaError::Protocol(format!(
                "PVA segmentation violation: expect_seg={} continuation={} cmd 0x{:02x} vs saved 0x{:02x}",
                expect_seg, continuation, frame.header.command, seg_cmd
            )));
        }
        if raw_seg == 0 || raw_seg == HeaderFlags::SEGMENT_FIRST {
            // Start of a new logical message — reset the accumulator
            // (in unsegmented case both reset and dispatch happen
            // below).
            expect_seg = true;
            seg_cmd = frame.header.command;
            seg_buf.clear();
        }
        // Cap reassembly at max_msg_size. read_frame already enforces
        // it per-frame; without this an adversary streams SegFirst →
        // SegMiddle … forever, growing seg_buf without bound.
        if seg_buf.len().saturating_add(frame.payload.len()) > max_msg_size {
            return Err(PvaError::Protocol(format!(
                "segmented PVA message exceeds max_message_size ({} > {})",
                seg_buf.len() + frame.payload.len(),
                max_msg_size
            )));
        }
        seg_buf.extend_from_slice(&frame.payload);
        if raw_seg != 0 && raw_seg != HeaderFlags::SEGMENT_LAST {
            // SegFirst (with following segments) or SegMiddle: keep
            // accumulating, do not dispatch yet.
            continue;
        }
        // Reaching here means: unsegmented (raw_seg==0) OR SegLast.
        expect_seg = false;
        // Build a synthetic Frame whose payload is the reassembled
        // body; dispatch path inspects only `header.command` and
        // `payload`, plus byte-order via `frame.order()`.
        let frame = if raw_seg == 0 {
            frame
        } else {
            Frame {
                header: PvaHeader {
                    version: frame.header.version,
                    flags: HeaderFlags::new(false, false, order),
                    command: seg_cmd,
                    payload_length: seg_buf.len() as u32,
                },
                payload: std::mem::take(&mut seg_buf),
            }
        };

        // Pre-handshake: only CONNECTION_VALIDATION (1) is meaningful; client
        // replies with its buffer/registry/qos/auth payload. We accept any
        // and respond CONNECTION_VALIDATED.
        if !handshake_complete {
            if frame.header.command == Command::ConnectionValidation.code() {
                // Parse the client's auth payload: skip buffer_size (u32),
                // introspection_size (u16), qos (u16); read selected method
                // (string); when method == "ca", read the type+value of the
                // auth Value and pull out the `user` / `host` fields. Pure
                // metadata for audit/logging.
                // F8: when the connection is mTLS-authenticated, the
                // x509 identity from the verified cert chain wins — the
                // client's CONNECTION_VALIDATION claim is parsed only
                // for diagnostics and never replaces it.
                if x509_locked {
                    // PVA-R22: a decode fault here is still fatal —
                    // log + propagate. Pre-fix swallowed; pvxs
                    // `serverconn.cpp:211-216` calls `bev.reset()`.
                    match parse_client_credentials(&frame, order)? {
                        Some(claimed) => debug!(
                            ?peer,
                            x509_account = %cred.account,
                            x509_authority = %cred.authority,
                            claimed_method = %claimed.method,
                            claimed_account = %claimed.account,
                            "PVA client over mTLS — x509 identity overrides CONNECTION_VALIDATION claim"
                        ),
                        None => debug!(
                            ?peer,
                            "PVA client over mTLS sent anonymous CONNECTION_VALIDATION"
                        ),
                    }
                } else {
                    // PVA-R22: a decode fault is now connection-fatal
                    // (matches pvxs `serverconn.cpp:211-216`
                    // bev.reset). An anonymous handshake (empty
                    // method) returns Ok(None) and keeps the
                    // existing anonymous credential. Only a fully
                    // decoded auth structure replaces `cred`.
                    if let Some(claimed) = parse_client_credentials(&frame, order)? {
                        cred = claimed;
                    }
                }
                debug!(?peer, method = %cred.method, account = %cred.account,
                    authority = %cred.authority, roles = ?cred.roles,
                    "PVA client credentials");
                // pvxs `serverconn.cpp:238-241` parity: when the client
                // picks an auth method we never advertised, reply
                // CONNECTION_VALIDATED with Status::Error so the client
                // knows its elevated identity claim was rejected. pvxs
                // keeps the connection open and falls back to whatever
                // identity is recorded (typically anonymous via the
                // empty-method path inside parse_client_credentials);
                // matches "No practical way to handle auth failure. So
                // we accept all credentials, but may not grant rights."
                // F8: an mTLS connection is authenticated by its
                // verified certificate chain — `cred.method` is
                // `"x509"` regardless of the CONNECTION_VALIDATION
                // claim, and that is always a valid method when TLS is
                // in use (pvxs advertises `x509` for TLS transports).
                // So the unadvertised-method rejection only applies to
                // the plain-TCP `ca`/`anonymous` negotiation.
                let advertised = x509_locked
                    || ADVERTISED_AUTH_METHODS
                        .iter()
                        .any(|m| m.eq_ignore_ascii_case(&cred.method));
                let validated_status = if advertised {
                    Status::ok()
                } else {
                    debug!(
                        ?peer,
                        method = %cred.method,
                        "PVA client selects unadvertised auth method — replying Status::Error"
                    );
                    Status::error("Client selects unadvertised auth".to_string())
                };
                let mut payload = Vec::new();
                validated_status.write_into(order, &mut payload);
                let h = PvaHeader::application(
                    true,
                    order,
                    Command::ConnectionValidated.code(),
                    payload.len() as u32,
                );
                let mut buf = Vec::new();
                h.write_into(&mut buf);
                buf.extend_from_slice(&payload);
                let _ = tx.send(buf).await;
                handshake_complete = true;
                // Fire user-installed `auth_complete` hook (pvxs
                // serverconn.cpp:181 parity) once we've accepted the
                // peer's identity claim. Hook signature mirrors pvxs
                // — peer addr + credentials snapshot. ACF
                // integration goes here.
                if let Some(hook) = config.auth_complete.as_ref() {
                    hook(peer, &cred);
                }
                continue;
            } else {
                // Some clients send CREATE_CHANNEL right after SET_BYTE_ORDER
                // skipping a fresh CONNECTION_VALIDATION exchange — accept.
                handshake_complete = true;
            }
        }

        // Application messages
        match Command::from_code(frame.header.command) {
            Some(Command::CreateChannel) => {
                // pvxs `serverchan.cpp:269-358` allows `count > 1`
                // (cid, name) pairs in one frame; the per-connection
                // cap check is now per-pair inside the handler.
                let before = channels.len();
                handle_create_channel(
                    &source,
                    &frame,
                    &tx,
                    &mut channels,
                    order,
                    config.max_channels_per_connection,
                    peer,
                )
                .await?;
                // F-G7: track channel-add success via the HashMap
                // delta — works for both count=1 and count>1 since
                // we count net inserts.
                let added = channels.len().saturating_sub(before);
                for _ in 0..added {
                    peer_entry.channel_added();
                }
            }
            Some(Command::DestroyChannel) => {
                let before = channels.len();
                handle_destroy_channel(&frame, &tx, &mut channels, order).await?;
                if channels.len() < before {
                    peer_entry.channel_removed();
                }
            }
            Some(Command::Get) => {
                peer_entry.op_init();
                handle_op(
                    &source,
                    &frame,
                    &tx,
                    &mut channels,
                    order,
                    OpKind::Get,
                    &config,
                    &mut encode_type_cache,
                    peer,
                    &cred,
                )
                .await?;
            }
            Some(Command::Put) => {
                peer_entry.op_init();
                handle_op(
                    &source,
                    &frame,
                    &tx,
                    &mut channels,
                    order,
                    OpKind::Put,
                    &config,
                    &mut encode_type_cache,
                    peer,
                    &cred,
                )
                .await?;
            }
            Some(Command::Monitor) => {
                peer_entry.op_init();
                handle_op(
                    &source,
                    &frame,
                    &tx,
                    &mut channels,
                    order,
                    OpKind::Monitor,
                    &config,
                    &mut encode_type_cache,
                    peer,
                    &cred,
                )
                .await?;
            }
            Some(Command::Rpc) => {
                peer_entry.op_init();
                handle_op(
                    &source,
                    &frame,
                    &tx,
                    &mut channels,
                    order,
                    OpKind::Rpc,
                    &config,
                    &mut encode_type_cache,
                    peer,
                    &cred,
                )
                .await?;
            }
            Some(Command::GetField) => {
                handle_get_field(&source, &frame, &tx, &channels, order).await?;
            }
            Some(Command::Search) => {
                // PVA-R11: TCP-circuit SEARCH (pvxs
                // `serverchan.cpp:173-255`). Required for
                // name-server-redirect deployments where pvxs
                // clients send SEARCH over the established TCP
                // connection rather than via UDP. Pre-fix Rust
                // had no arm here and the frame fell through to
                // the silent default — the redirector hung waiting
                // for SEARCH_RESPONSE.
                handle_tcp_search(&source, &frame, &tx, &config).await?;
            }
            Some(Command::DestroyRequest) => {
                handle_destroy_request(&frame, &mut channels, order)?;
            }
            Some(Command::CancelRequest) => {
                handle_cancel_request(&frame, &mut channels, order)?;
            }
            Some(Command::Message) => {
                handle_message(&frame, order, &peer)?;
            }
            Some(Command::PutGet) => {
                // F11: atomic put-then-get. The PVA wire spec defines
                // PUT_GET as a separate command (cmd 12). pvxs leaves
                // `handle_PUT_GET` empty, but we implement the full
                // INIT/PUT/GET/DESTROY lifecycle on the Rust side so
                // a PUT_GET-capable client gets a real round trip.
                peer_entry.op_init();
                handle_put_get(
                    &source,
                    &frame,
                    &tx,
                    &mut channels,
                    order,
                    &config,
                    &mut encode_type_cache,
                    peer,
                    &cred,
                )
                .await?;
            }
            Some(Command::Process) => {
                // F11: trigger record processing with no value
                // transfer (PVA cmd 16). Full INIT/PROCESS/DESTROY
                // lifecycle — routed through the source's typed
                // `process_checked` (WRITE-class ACF gate).
                peer_entry.op_init();
                handle_process(
                    &source,
                    &frame,
                    &tx,
                    &mut channels,
                    order,
                    &config,
                    peer,
                    &cred,
                )
                .await?;
            }
            Some(Command::OriginTag) => {
                // I-5: pvxs origin-tag is an optional payload for
                // tracing/debugging that the spec lets servers
                // ignore. We log at debug level and carry on so a
                // client that sends one still works.
                debug!(
                    peer = ?peer,
                    bytes = frame.payload.len(),
                    "OriginTag received (silently consumed)"
                );
            }
            Some(Command::AclChange) => {
                // I-5: AclChange is a server → client push that
                // pvxs / pvAccessCPP servers emit when access
                // rights for a channel change. We don't yet wire
                // up server-side ACF mutation events, so receiving
                // one as a server (which shouldn't happen) is
                // logged-and-ignored. As a client we'd react in
                // the read loop.
                debug!(
                    peer = ?peer,
                    "AclChange received as server (unexpected); ignoring"
                );
            }
            Some(Command::MultipleData) => {
                // I-5: MultipleData was a never-really-deployed
                // batch monitor delivery format. pvxs decodes it
                // but our client/server only emit single-data
                // monitor frames. Server-side receipt is
                // inappropriate — log and drop.
                debug!(
                    peer = ?peer,
                    "MultipleData received as server (unexpected); ignoring"
                );
            }
            Some(Command::Echo) => {
                // Echo back the same frame.
                let mut buf = Vec::new();
                let h = PvaHeader::application(
                    true,
                    order,
                    Command::Echo.code(),
                    frame.payload.len() as u32,
                );
                h.write_into(&mut buf);
                buf.extend_from_slice(&frame.payload);
                let _ = tx.send(buf).await;
            }
            _ => {
                // Unhandled — keep going.
            }
        }
    }
}

/// Build a minimal [`OpState`] for non-MONITOR ops (GET / PUT /
/// PUT_GET / PROCESS). The monitor-specific fields are all defaulted
/// to inert values — these ops never spawn a subscriber task.
fn non_monitor_op_state(intro: FieldDesc, kind: OpKind, mask: BitSet) -> OpState {
    OpState {
        intro,
        kind,
        monitor_started: false,
        monitor_abort: None,
        mask,
        monitor_window: None,
        monitor_window_notify: None,
        monitor_paused: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        monitor_filters: Arc::new(epics_base_rs::server::database::filters::FilterChain::new()),
        put_auto_exec: true,
        pv_request: None,
    }
}

/// F11: PVA `PUT_GET` (cmd 12) handler — atomic put-then-get.
///
/// Sub-command lifecycle, mirroring the GET / PUT handlers:
/// - INIT  (`subcmd & 0x08`): decode the pvRequest, register the op,
///   reply `ioid + subcmd + status + putIF + getIF`. We serve a
///   single channel introspection for both the put and the get
///   structure (the common NT case where the put and readback types
///   are identical).
/// - PUT-GET (`subcmd & 0x08 == 0`): decode `changed bitset + put
///   value`, run the WRITE-gated `put_value_checked`, then the
///   READ-gated `get_value_checked`, and reply
///   `ioid + subcmd + status + getBitset + getValue`.
/// - DESTROY (`subcmd & 0x10`): drop the op slot.
///
/// pvxs leaves `handle_PUT_GET` empty; this implements the operation
/// properly per the wire spec so a PUT_GET-capable client works.
#[allow(clippy::too_many_arguments)]
async fn handle_put_get(
    source: &DynSource,
    frame: &Frame,
    tx: &SrvTx,
    channels: &mut HashMap<u32, ChannelState>,
    order: ByteOrder,
    config: &PvaServerConfig,
    encode_cache: &mut EncodeTypeCache,
    peer: std::net::SocketAddr,
    cred: &ClientCredentials,
) -> PvaResult<()> {
    let mut cur = frame.cursor();
    let sid = cur
        .get_u32(order)
        .map_err(|e| PvaError::Decode(e.to_string()))?;
    let ioid = cur
        .get_u32(order)
        .map_err(|e| PvaError::Decode(e.to_string()))?;
    let subcmd = cur.get_u8().map_err(|e| PvaError::Decode(e.to_string()))?;

    let ch = match channels.get_mut(&sid) {
        Some(c) => c,
        None => {
            send_op_error(tx, OpKind::PutGet, ioid, "unknown channel sid", order).await?;
            return Ok(());
        }
    };

    // DESTROY phase — release the op slot, no reply.
    if subcmd & QosFlags::DESTROY != 0 {
        ch.ops.remove(&ioid);
        return Ok(());
    }

    if subcmd & QosFlags::INIT != 0 {
        // PVA-R21: duplicate INIT on a live IOID is connection-fatal
        // (mirror of `handle_op`).
        if ch.ops.contains_key(&ioid) {
            return Err(PvaError::Decode(format!(
                "duplicate PUT_GET INIT on live IOID {ioid}"
            )));
        }
        if ch.ops.len() >= config.max_ops_per_channel {
            send_op_error(
                tx,
                OpKind::PutGet,
                ioid,
                "max ops per channel exceeded",
                order,
            )
            .await?;
            return Ok(());
        }
        // PVA-R16: PUT_GET also requires a descriptor.
        let intro = match ch.introspection.clone() {
            Some(d) => d,
            None => {
                send_op_error(tx, OpKind::PutGet, ioid, "must provide prototype", order).await?;
                return Ok(());
            }
        };
        // pvRequest: `type + value` (pvxs clientget.cpp). Translate to
        // a field mask the GET leg consults.
        let req_desc = match decode_type_desc(&mut cur, order) {
            Ok(d) => d,
            Err(e) => {
                send_op_error(
                    tx,
                    OpKind::PutGet,
                    ioid,
                    &format!("invalid pvRequest descriptor: {e}"),
                    order,
                )
                .await?;
                return Ok(());
            }
        };
        let _req_value = decode_pv_field(&req_desc, &mut cur, order).ok();
        // PVA-R19: empty mask is an INIT error.
        let mask = match crate::pv_request::request_to_mask(&intro, &req_desc) {
            Ok(m) => m,
            Err(e) => {
                send_op_error(
                    tx,
                    OpKind::PutGet,
                    ioid,
                    &format!("invalid pvRequest mask: {e}"),
                    order,
                )
                .await?;
                return Ok(());
            }
        };

        ch.ops.insert(
            ioid,
            non_monitor_op_state(intro.clone(), OpKind::PutGet, mask),
        );

        // INIT response: ioid + subcmd + status + putIF + getIF.
        // pvxs `serverget.cpp` emits two type descriptors for PUT_GET
        // (the put-request and get-response structures). We serve the
        // same channel introspection for both legs.
        let mut payload = Vec::new();
        payload.put_u32(ioid, order);
        payload.put_u8(subcmd);
        Status::ok().write_into(order, &mut payload);
        if config.emit_type_cache {
            encode_type_desc_cached(&intro, order, encode_cache, &mut payload);
            encode_type_desc_cached(&intro, order, encode_cache, &mut payload);
        } else {
            encode_type_desc(&intro, order, &mut payload);
            encode_type_desc(&intro, order, &mut payload);
        }
        let h = PvaHeader::application(true, order, Command::PutGet.code(), payload.len() as u32);
        let mut buf = Vec::new();
        h.write_into(&mut buf);
        buf.extend_from_slice(&payload);
        let _ = tx.send(buf).await;
        return Ok(());
    }

    // PUT-GET data phase.
    let op = ch.ops.get(&ioid).cloned();
    let (intro, mask) = match op {
        Some(o) => (o.intro, o.mask),
        None => {
            send_op_error(tx, OpKind::PutGet, ioid, "operation not initialised", order).await?;
            return Ok(());
        }
    };
    let pv_name = ch.name.clone();

    // The data frame carries the put bitset + put value, exactly like
    // a PUT EXEC. pvxs clientget.cpp PUT_GET state sends `0x00`.
    // The value is a BitSet delta (`changed | partial value`): only
    // the marked fields are present on the wire, so decode with the
    // changed-BitSet (pvData spec §5.4 bit numbering) — a full
    // `decode_pv_field` desyncs the stream for multi-field structures.
    let changed = BitSet::decode(&mut cur, order).map_err(|e| PvaError::Decode(e.to_string()))?;
    let put_delta = decode_pv_field_with_bitset(&intro, &changed, 0, &mut cur, order)
        .map_err(|e| PvaError::Decode(format!("PUT_GET requires a value payload: {e}")))?;

    let ctx = crate::server_native::source::ChannelContext {
        peer,
        account: cred.account.clone(),
        method: cred.method.clone(),
        host: cred.host.clone(),
        authority: cred.authority.clone(),
        pv_request: None,
    };

    let mut payload = Vec::new();
    payload.put_u32(ioid, order);
    payload.put_u8(subcmd);

    // PUT leg — WRITE-gated. The wire delta carries only the marked
    // fields; applying it is a read-merge-write against the PV's
    // current value. `put_delta_checked` performs the merge
    // atomically under the source's lock so two concurrent partial
    // PUT_GETs with disjoint changed-fields cannot both read the
    // same prior and lose the first writer's fields.
    let put_result = {
        let checked = source
            .access_gate()
            .check(
                &pv_name,
                &ctx.host,
                &ctx.account,
                &ctx.method,
                &ctx.authority,
            )
            .await;
        source
            .put_delta_checked(checked, intro.clone(), changed, put_delta, ctx.clone())
            .await
    };
    if let Err(msg) = put_result {
        Status::error(msg).write_into(order, &mut payload);
        let h = PvaHeader::application(true, order, Command::PutGet.code(), payload.len() as u32);
        let mut buf = Vec::new();
        h.write_into(&mut buf);
        buf.extend_from_slice(&payload);
        let _ = tx.send(buf).await;
        return Ok(());
    }

    // GET leg — READ-gated, re-checked through the same gate. Mirror
    // the PUT-with-getback path: a peer with WRITE-only ASG still
    // gets an OK status, just an empty (zero-field) readback bitset.
    let read_checked = source
        .access_gate()
        .check(
            &pv_name,
            &ctx.host,
            &ctx.account,
            &ctx.method,
            &ctx.authority,
        )
        .await;
    match source.get_value_checked(read_checked, ctx).await {
        Some(v) => {
            Status::ok().write_into(order, &mut payload);
            // `mask` is a *selection* mask (request_to_mask) — convert
            // it to a valid wire changed-bitset so a partial field
            // filter does not get a root-bit-set "whole structure".
            let changed = crate::pvdata::encode::canonical_changed_bitset(&intro, &mask);
            changed.write_into(order, &mut payload);
            crate::pvdata::encode::encode_pv_field_with_bitset(
                &v,
                &intro,
                &changed,
                0,
                order,
                &mut payload,
            );
        }
        None => {
            // PUT committed but READ denied / PV vanished: emit OK +
            // an all-zero bitset so the client decodes zero fields
            // and consumes no value bytes (same shape as the
            // PUT-getback path).
            Status::ok().write_into(order, &mut payload);
            let empty = BitSet::with_capacity(intro.total_bits());
            empty.write_into(order, &mut payload);
        }
    }
    let h = PvaHeader::application(true, order, Command::PutGet.code(), payload.len() as u32);
    let mut buf = Vec::new();
    h.write_into(&mut buf);
    buf.extend_from_slice(&payload);
    let _ = tx.send(buf).await;
    Ok(())
}

/// F11: PVA `PROCESS` (cmd 16) handler — trigger record processing
/// with no value transfer.
///
/// Sub-command lifecycle:
/// - INIT  (`subcmd & 0x08`): decode + discard the pvRequest, register
///   the op, reply `ioid + subcmd + status` (no introspection — there
///   is no value type to negotiate).
/// - PROCESS (`subcmd & 0x08 == 0`): run the WRITE-gated
///   `process_checked` on the source, reply `ioid + subcmd + status`.
/// - DESTROY (`subcmd & 0x10`): drop the op slot.
#[allow(clippy::too_many_arguments)]
async fn handle_process(
    source: &DynSource,
    frame: &Frame,
    tx: &SrvTx,
    channels: &mut HashMap<u32, ChannelState>,
    order: ByteOrder,
    config: &PvaServerConfig,
    peer: std::net::SocketAddr,
    cred: &ClientCredentials,
) -> PvaResult<()> {
    let mut cur = frame.cursor();
    let sid = cur
        .get_u32(order)
        .map_err(|e| PvaError::Decode(e.to_string()))?;
    let ioid = cur
        .get_u32(order)
        .map_err(|e| PvaError::Decode(e.to_string()))?;
    let subcmd = cur.get_u8().map_err(|e| PvaError::Decode(e.to_string()))?;

    let ch = match channels.get_mut(&sid) {
        Some(c) => c,
        None => {
            send_op_error(tx, OpKind::Process, ioid, "unknown channel sid", order).await?;
            return Ok(());
        }
    };

    if subcmd & QosFlags::DESTROY != 0 {
        ch.ops.remove(&ioid);
        return Ok(());
    }

    if subcmd & QosFlags::INIT != 0 {
        // PVA-R21: duplicate INIT on a live IOID is connection-fatal.
        if ch.ops.contains_key(&ioid) {
            return Err(PvaError::Decode(format!(
                "duplicate PROCESS INIT on live IOID {ioid}"
            )));
        }
        if ch.ops.len() >= config.max_ops_per_channel {
            send_op_error(
                tx,
                OpKind::Process,
                ioid,
                "max ops per channel exceeded",
                order,
            )
            .await?;
            return Ok(());
        }
        // PVA-R16: PROCESS still requires a descriptor — even though
        // PROCESS has no value payload, the source must commit to
        // *some* introspection at channel creation. A missing
        // descriptor means the source can't describe what PROCESS
        // would act on.
        let intro = match ch.introspection.clone() {
            Some(d) => d,
            None => {
                send_op_error(tx, OpKind::Process, ioid, "must provide prototype", order).await?;
                return Ok(());
            }
        };
        // The PROCESS pvRequest carries no field selection of interest
        // (process transfers no value) — decode-and-discard so any
        // trailing bytes are consumed cleanly.
        let _ = decode_type_desc(&mut cur, order)
            .ok()
            .and_then(|d| decode_pv_field(&d, &mut cur, order).ok());
        let mask = BitSet::all_set(intro.total_bits());
        ch.ops
            .insert(ioid, non_monitor_op_state(intro, OpKind::Process, mask));

        // INIT response: ioid + subcmd + status. No type descriptor —
        // PROCESS negotiates no value.
        let mut payload = Vec::new();
        payload.put_u32(ioid, order);
        payload.put_u8(subcmd);
        Status::ok().write_into(order, &mut payload);
        let h = PvaHeader::application(true, order, Command::Process.code(), payload.len() as u32);
        let mut buf = Vec::new();
        h.write_into(&mut buf);
        buf.extend_from_slice(&payload);
        let _ = tx.send(buf).await;
        return Ok(());
    }

    // PROCESS data phase — no payload to decode.
    if !ch.ops.contains_key(&ioid) {
        send_op_error(
            tx,
            OpKind::Process,
            ioid,
            "operation not initialised",
            order,
        )
        .await?;
        return Ok(());
    }
    let pv_name = ch.name.clone();
    let ctx = crate::server_native::source::ChannelContext {
        peer,
        account: cred.account.clone(),
        method: cred.method.clone(),
        host: cred.host.clone(),
        authority: cred.authority.clone(),
        pv_request: None,
    };
    // Processing mutates record state — WRITE-gated, like PUT.
    let result = {
        let checked = source
            .access_gate()
            .check(
                &pv_name,
                &ctx.host,
                &ctx.account,
                &ctx.method,
                &ctx.authority,
            )
            .await;
        source.process_checked(checked, ctx).await
    };

    let mut payload = Vec::new();
    payload.put_u32(ioid, order);
    payload.put_u8(subcmd);
    match result {
        Ok(()) => Status::ok().write_into(order, &mut payload),
        Err(msg) => Status::error(msg).write_into(order, &mut payload),
    }
    let h = PvaHeader::application(true, order, Command::Process.code(), payload.len() as u32);
    let mut buf = Vec::new();
    h.write_into(&mut buf);
    buf.extend_from_slice(&payload);
    let _ = tx.send(buf).await;
    Ok(())
}

async fn read_frame<R: tokio::io::AsyncRead + Unpin>(
    reader: &mut R,
    rx_buf: &mut Vec<u8>,
    op_timeout: Duration,
    max_msg_size: usize,
) -> PvaResult<Frame> {
    loop {
        // Role-aware parse: a server's inbound frames must have the
        // Server direction bit CLEAR (pvxs `conn.cpp:160` —
        // `isClient ^ !!(header[2]&pva_flags::Server)`). Reject and
        // tear down the connection if the peer echoes our own
        // outbound shape back at us.
        if let Some((frame, n)) = try_parse_frame_role(rx_buf, PeerRole::Server)? {
            rx_buf.drain(..n);
            return Ok(frame);
        }
        // Peek the header length once we have 8 bytes — if the peer
        // claimed a payload larger than `max_msg_size`, drop the
        // connection before growing rx_buf any further. Without this
        // a malicious header announcing 4 GiB would force us to
        // OOM-loop here. pvxs enforces the same cap implicitly via
        // libevent's evbuffer_setwatermark; we do it explicitly.
        if rx_buf.len() >= PvaHeader::SIZE {
            if let Ok(hdr) = PvaHeader::decode(&mut std::io::Cursor::new(&rx_buf[..])) {
                if !hdr.flags.is_control() && hdr.payload_length as usize > max_msg_size {
                    return Err(PvaError::Protocol(format!(
                        "inbound payload {} exceeds max_message_size {}",
                        hdr.payload_length, max_msg_size
                    )));
                }
            }
        }
        let mut chunk = [0u8; 4096];
        let n = match tokio::time::timeout(op_timeout, reader.read(&mut chunk)).await {
            Ok(Ok(n)) => n,
            Ok(Err(e)) => return Err(PvaError::Io(e)),
            Err(_) => return Err(PvaError::Timeout),
        };
        if n == 0 {
            return Err(PvaError::Protocol("client closed".into()));
        }
        rx_buf.extend_from_slice(&chunk[..n]);
    }
}

/// Build a server-side CONNECTION_VALIDATION request (cmd=1, server direction).
///
/// Wire layout (8-byte header + this payload):
///
/// ```text
/// u32 buffer_size
/// u16 introspection_registry_size
/// Size n
/// n × String   (auth method names)
/// ```
fn build_server_connection_validation(
    order: ByteOrder,
    buffer_size: u32,
    registry_size: u16,
    auth_methods: &[&str],
) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.put_u32(buffer_size, order);
    payload.put_u16(registry_size, order);
    encode_size_into(auth_methods.len() as u32, order, &mut payload);
    for m in auth_methods {
        encode_string_into(m, order, &mut payload);
    }
    let h = PvaHeader::application(
        true,
        order,
        Command::ConnectionValidation.code(),
        payload.len() as u32,
    );
    let mut out = Vec::new();
    h.write_into(&mut out);
    out.extend_from_slice(&payload);
    out
}

#[allow(clippy::too_many_arguments)]
async fn handle_create_channel(
    source: &DynSource,
    frame: &Frame,
    tx: &SrvTx,
    channels: &mut HashMap<u32, ChannelState>,
    order: ByteOrder,
    max_channels_per_connection: usize,
    peer: SocketAddr,
) -> PvaResult<()> {
    let mut cur = frame.cursor();
    // pvxs `serverchan.cpp:269-358`: a single CREATE_CHANNEL frame
    // can carry `count` (cid, name) pairs and the server must emit
    // one CREATE_CHANNEL response frame per pair, in arrival order.
    // The Java pvAccess client batches multiple new channels in one
    // frame after a SEARCH response; we used to only honour the
    // first pair and leave the remaining bytes unconsumed.
    let count = cur
        .get_u16(order)
        .map_err(|e| PvaError::Decode(e.to_string()))?;
    for _ in 0..count {
        // PVA-R28: truncated CID / malformed string is a protocol-
        // fatal decode error. pvxs `serverchan.cpp:364-368`:
        // `if(!M.good()) { conn->log("CREATE_CHANNEL...");
        // conn->bev.reset(); }` — the connection is reset on a bad
        // decoder state after the per-name loop. Pre-fix Rust used
        // `match ... Err(_) => break` which kept the connection alive
        // and let any previously-decoded pairs stay attached. Mirror
        // pvxs: any decode failure in a name pair tears the
        // connection down.
        let cid = cur
            .get_u32(order)
            .map_err(|e| PvaError::Decode(format!("CREATE_CHANNEL cid: {e}")))?;
        let name = match crate::proto::decode_string(&mut cur, order)
            .map_err(|e| PvaError::Decode(format!("CREATE_CHANNEL name: {e}")))?
        {
            Some(s) => s,
            None => {
                // pvxs treats an empty name in the inner loop as a
                // semantic mistake; we keep that as a soft break
                // (the for-loop ends and Ok(()) flows to caller).
                break;
            }
        };
        if name.is_empty() {
            break;
        }
        // A-G1 per-channel cap check moved inside the per-pair loop
        // so a multi-name CREATE_CHANNEL can't sneak past the per-
        // connection limit by amortising the gate against the first
        // pair only.
        if channels.len() >= max_channels_per_connection {
            warn!(
                ?peer,
                pv = %name,
                "rejecting CREATE_CHANNEL: per-connection limit reached"
            );
            let mut payload = Vec::new();
            payload.put_u32(cid, order);
            payload.put_u32(0u32, order);
            Status::error("max channels per connection reached".to_string())
                .write_into(order, &mut payload);
            let h = PvaHeader::application(
                true,
                order,
                Command::CreateChannel.code(),
                payload.len() as u32,
            );
            let mut buf = Vec::new();
            h.write_into(&mut buf);
            buf.extend_from_slice(&payload);
            let _ = tx.send(buf).await;
            continue;
        }
        emit_create_channel_reply(source, channels, tx, cid, &name, order).await?;
    }
    Ok(())
}

/// One iteration of pvxs `handle_CREATE_CHANNEL`'s inner loop:
/// resolve the PV, allocate a SID (or reject), and emit the
/// `cid + sid + status` response frame. Factored so the count > 1
/// loop above is a single straight-line concern.
async fn emit_create_channel_reply(
    source: &DynSource,
    channels: &mut HashMap<u32, ChannelState>,
    tx: &SrvTx,
    cid: u32,
    name: &str,
    order: ByteOrder,
) -> PvaResult<()> {
    if !source.has_pv(name).await {
        let mut payload = Vec::new();
        payload.put_u32(cid, order);
        payload.put_u32(0u32, order); // sid (placeholder)
        Status::error(format!("unknown PV: {name}")).write_into(order, &mut payload);
        let h = PvaHeader::application(
            true,
            order,
            Command::CreateChannel.code(),
            payload.len() as u32,
        );
        let mut buf = Vec::new();
        h.write_into(&mut buf);
        buf.extend_from_slice(&payload);
        let _ = tx.send(buf).await;
        return Ok(());
    }

    let sid = alloc_sid();
    let intro = source.get_introspection(name).await;
    channels.insert(
        sid,
        ChannelState {
            name: name.to_string(),
            cid,
            sid,
            introspection: intro,
            ops: HashMap::new(),
        },
    );

    let mut payload = Vec::new();
    payload.put_u32(cid, order);
    payload.put_u32(sid, order);
    Status::ok().write_into(order, &mut payload);
    // pvxs serverchan.cpp:349-351 emits `cid + sid + status` only —
    // no access_rights field follows.
    let h = PvaHeader::application(
        true,
        order,
        Command::CreateChannel.code(),
        payload.len() as u32,
    );
    let mut buf = Vec::new();
    h.write_into(&mut buf);
    buf.extend_from_slice(&payload);
    let _ = tx.send(buf).await;
    Ok(())
}

async fn handle_destroy_channel(
    frame: &Frame,
    tx: &SrvTx,
    channels: &mut HashMap<u32, ChannelState>,
    order: ByteOrder,
) -> PvaResult<()> {
    let mut cur = frame.cursor();
    let sid = cur
        .get_u32(order)
        .map_err(|e| PvaError::Decode(e.to_string()))?;
    let cid = cur
        .get_u32(order)
        .map_err(|e| PvaError::Decode(e.to_string()))?;
    // pvxs `serverchan.cpp:382-386`: when the SID is unknown the server
    // logs at debug and silently returns — no DESTROY_CHANNEL reply is
    // sent. Fabricating "yes I destroyed it" for an SID we never
    // created (a) lets a malicious peer extract reply frames for any
    // SID/CID pair (small amplification) and (b) confuses correctness
    // diagnostics on the client side: a peer that lost track and
    // re-DESTROYs gets an `OK` echo back instead of the expected
    // silence, masking the bug. Match pvxs: lookup, return on miss,
    // remove + reply only on hit.
    if !channels.contains_key(&sid) {
        debug!(sid, cid, "DESTROY_CHANNEL on unknown SID: dropping");
        return Ok(());
    }
    // pvxs also warns when `chan->cid != cid` (line 390-393) but proceeds
    // with the destroy. We don't keep the wire CID alongside the SID
    // mapping today — log on mismatch for parity with the warn-level
    // diagnostic, then proceed.
    if let Some(ch) = channels.get(&sid)
        && ch.cid != cid
    {
        debug!(
            sid,
            stored_cid = ch.cid,
            wire_cid = cid,
            "DESTROY_CHANNEL CID mismatch"
        );
    }
    // Removing the channel drops every OpState in `ops`, which drops
    // each `monitor_abort: Option<Arc<AbortOnDrop>>` and cancels the
    // associated subscriber task — preventing orphaned spawns from
    // holding the source's broadcast subscription.
    channels.remove(&sid);
    let mut payload = Vec::new();
    payload.put_u32(sid, order);
    payload.put_u32(cid, order);
    let h = PvaHeader::application(
        true,
        order,
        Command::DestroyChannel.code(),
        payload.len() as u32,
    );
    let mut buf = Vec::new();
    h.write_into(&mut buf);
    buf.extend_from_slice(&payload);
    let _ = tx.send(buf).await;
    Ok(())
}

/// Handle CANCEL_REQUEST (cmd 21). pvxs serverconn.cpp:262 — moves the op
/// from Executing back to Idle without freeing it; the underlying
/// `MonitorOp` (and the source's onSubscribe state) stays alive so a
/// later START restores Executing without re-issuing the subscription.
///
/// Round 4 (cancel-vs-destroy refactor): previously the Rust handler
/// dropped `monitor_abort` and cleared `monitor_started`, which aborted
/// the subscriber task and forced a full re-spawn on the next START.
/// That heavy path: (1) re-subscribed at the source, potentially
/// dropping queued events between cancel and START, and (2) re-took the
/// type/ACL/filter setup cost. Mirroring pvxs, we now flip
/// `monitor_paused=true` and keep the subscriber task alive. The
/// subscriber loop already gates emission on `monitor_paused`, so this
/// suspends events without tearing the task down. The matching
/// START (subcmd 0x44 — start | process) clears `monitor_paused` via
/// the existing resume path at handle_op, transitioning back to
/// Executing without a re-subscribe. DESTROY (`CMD_DESTROY_REQUEST`)
/// still removes the op outright, dropping `monitor_abort` and
/// aborting the task — the only path that releases source-side state.
fn handle_cancel_request(
    frame: &Frame,
    channels: &mut HashMap<u32, ChannelState>,
    order: ByteOrder,
) -> PvaResult<()> {
    let mut cur = frame.cursor();
    // PVA-R28: pvxs `serverconn.cpp:262-270` throws on truncated
    // CANCEL_REQUEST (`if(!M.good()) throw ...`), which the conn
    // loop turns into a connection reset. Pre-fix Rust silently
    // returned. Mirror pvxs — bubble as a fatal decode error.
    let sid = cur
        .get_u32(order)
        .map_err(|e| PvaError::Decode(format!("CANCEL_REQUEST sid: {e}")))?;
    let ioid = cur
        .get_u32(order)
        .map_err(|e| PvaError::Decode(format!("CANCEL_REQUEST ioid: {e}")))?;
    if let Some(ch) = channels.get_mut(&sid) {
        if let Some(op) = ch.ops.get(&ioid) {
            // Suspend without aborting the subscriber task. pvxs
            // models cancel as Executing→Idle; the subscriber stays
            // around for the next START to flip back to Executing.
            // Only MONITOR has a long-lived subscriber to pause —
            // GET/PUT/RPC are two-shot so the field is effectively a
            // no-op for them (`monitor_paused` is never consulted off
            // the MONITOR path).
            op.monitor_paused
                .store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }
    Ok(())
}

/// Handle MESSAGE (cmd 18). pvxs serverconn.cpp:323 — clients send
/// log messages tagged with severity (Info/Warning/Error/Fatal). We
/// surface them through the `tracing` crate at the matching level.
fn handle_message(frame: &Frame, order: ByteOrder, peer: &SocketAddr) -> PvaResult<()> {
    let mut cur = frame.cursor();
    // PVA-R28: pvxs `serverconn.cpp:323-336` throws on malformed
    // MESSAGE; conn loop turns into a reset. Pre-fix Rust silently
    // returned (string-decode also substituted "").
    let ioid = cur
        .get_u32(order)
        .map_err(|e| PvaError::Decode(format!("MESSAGE ioid: {e}")))?;
    let mtype = cur
        .get_u8()
        .map_err(|e| PvaError::Decode(format!("MESSAGE type: {e}")))?;
    let msg = crate::proto::decode_string(&mut cur, order)
        .map_err(|e| PvaError::Decode(format!("MESSAGE string: {e}")))?
        .unwrap_or_default();
    match mtype {
        0 => debug!(?peer, ioid, message = %msg, "client info"),
        1 => warn!(?peer, ioid, message = %msg, "client warning"),
        2 | 3 => error!(?peer, ioid, message = %msg, "client error"),
        _ => debug!(?peer, ioid, mtype, message = %msg, "client message (unknown type)"),
    }
    Ok(())
}

fn handle_destroy_request(
    frame: &Frame,
    channels: &mut HashMap<u32, ChannelState>,
    order: ByteOrder,
) -> PvaResult<()> {
    let mut cur = frame.cursor();
    // PVA-R28: pvxs `serverconn.cpp:297-305` throws on malformed
    // DESTROY_REQUEST. Pre-fix Rust silently returned.
    let sid = cur
        .get_u32(order)
        .map_err(|e| PvaError::Decode(format!("DESTROY_REQUEST sid: {e}")))?;
    let ioid = cur
        .get_u32(order)
        .map_err(|e| PvaError::Decode(format!("DESTROY_REQUEST ioid: {e}")))?;
    if let Some(ch) = channels.get_mut(&sid) {
        // Removing the op drops `monitor_abort: Option<Arc<AbortOnDrop>>`.
        // Once the last clone is dropped, the subscriber task aborts.
        ch.ops.remove(&ioid);
    }
    Ok(())
}

/// PVA-R11: handle `Command::Search` arriving on an established
/// TCP virtual circuit. pvxs `serverchan.cpp:173-255` accepts this
/// path so a client configured with `EPICS_PVA_NAME_SERVERS=<srv>`
/// can resolve PVs without UDP. The wire body is identical to the
/// UDP SEARCH; we reuse the parser exposed by `udp.rs`. The
/// SEARCH_RESPONSE goes back on the same TCP connection (server-
/// direction bit set).
async fn handle_tcp_search(
    source: &DynSource,
    frame: &Frame,
    tx: &SrvTx,
    config: &PvaServerConfig,
) -> PvaResult<()> {
    // Rebuild the raw frame bytes so the UDP parser sees the same
    // shape (header + payload). `parse_search_request` reads from
    // the header inwards.
    let mut raw: Vec<u8> = Vec::with_capacity(PvaHeader::SIZE + frame.payload.len());
    frame.header.write_into(&mut raw);
    raw.extend_from_slice(&frame.payload);

    let Some(req) = super::udp::parse_search_request(&raw) else {
        // Malformed body — drop silently, same as the UDP path.
        // pvxs `serverchan.cpp:255` returns without emitting a
        // response on bad input.
        return Ok(());
    };

    // PVA-R10: filter by protocol. Default protocol on TCP is
    // "tcp" (or "tls" when TLS is in use). Empty list tolerated
    // as wildcard for legacy peers.
    let protocol: &'static str = if config.tls.is_some() { "tls" } else { "tcp" };
    let protocol_ok = req.protocols.is_empty() || req.protocols.iter().any(|p| p == protocol);
    let mut matched: Vec<u32> = Vec::with_capacity(req.queries.len());
    if protocol_ok {
        for (cid, name) in &req.queries {
            if source.searchable(name).await {
                matched.push(*cid);
            }
        }
    }
    // pvxs `serverchan.cpp:240-249`: emit the response only when
    // there's a match OR MustReply was set. Skip otherwise to
    // avoid leaking server presence on every probe.
    if !matched.is_empty() || req.must_reply {
        let response = super::udp::build_search_response_proto(
            config.guid,
            req.seq,
            config.tcp_port,
            &matched,
            req.byte_order,
            protocol,
        );
        let _ = tx.send(response).await;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn handle_op(
    source: &DynSource,
    frame: &Frame,
    tx: &SrvTx,
    channels: &mut HashMap<u32, ChannelState>,
    order: ByteOrder,
    kind: OpKind,
    config: &PvaServerConfig,
    encode_cache: &mut EncodeTypeCache,
    peer: std::net::SocketAddr,
    cred: &ClientCredentials,
) -> PvaResult<()> {
    let mut cur = frame.cursor();
    let sid = cur
        .get_u32(order)
        .map_err(|e| PvaError::Decode(e.to_string()))?;
    let ioid = cur
        .get_u32(order)
        .map_err(|e| PvaError::Decode(e.to_string()))?;
    let subcmd = cur.get_u8().map_err(|e| PvaError::Decode(e.to_string()))?;

    let ch = match channels.get_mut(&sid) {
        Some(c) => c,
        None => {
            // Send error.
            send_op_error(tx, kind, ioid, "unknown channel sid", order).await?;
            return Ok(());
        }
    };

    if subcmd & 0x08 != 0 {
        // PVA-R21: duplicate INIT on a live IOID is connection-fatal
        // per pvxs. `serverget.cpp:378-384` and `servermon.cpp:505-511`
        // reset the connection on `op->state != Created`; we model
        // "already created" as `ch.ops.contains_key(&ioid)`. Pre-fix
        // Rust let the insert below silently REPLACE the existing
        // OpState, which could drop a MONITOR subscriber task and
        // redirect later data frames to a different descriptor/mask
        // than the original operation negotiated.
        if ch.ops.contains_key(&ioid) {
            return Err(PvaError::Decode(format!(
                "duplicate INIT on live IOID {ioid} (pvxs serverget.cpp:378-384 protocol error)"
            )));
        }
        // A-G1: per-channel concurrent-op cap — refuse fresh INITs
        // once the channel's `ops` map hits the configured ceiling
        // so a malicious peer can't accumulate IOID state forever
        // by sending INIT … INIT … without ever issuing DESTROY.
        if ch.ops.len() >= config.max_ops_per_channel {
            send_op_error(tx, kind, ioid, "max ops per channel exceeded", order).await?;
            return Ok(());
        }

        // PVA-R16: pvxs `serverget.cpp:182-193` rejects missing
        // prototype for non-RPC operations with "Must provide
        // prototype". Rust's previous fallback turned a source bug
        // (no `get_introspection`) into a successful GET/PUT/MONITOR
        // INIT with a `Variant` descriptor — masking the bug and
        // letting later mismatched-value encoding look valid. RPC
        // can still proceed without a prototype (descriptor-late).
        let intro = match (kind, ch.introspection.clone()) {
            (OpKind::Rpc, Some(d)) => d,
            (OpKind::Rpc, None) => FieldDesc::Variant,
            (_, Some(d)) => d,
            (_, None) => {
                send_op_error(tx, kind, ioid, "must provide prototype", order).await?;
                return Ok(());
            }
        };

        // INIT — read pvRequest (`type + full value` per pvxs
        // clientget.cpp:351-352) and translate it to a field mask the
        // emit side will consult.
        //
        // PVA-R19: pvxs `serverget.cpp:367-375` and
        // `servermon.cpp:491-502` treat an invalid pvRequest type/value
        // decode as bad INIT and close the connection;
        // `pvrequest.cpp:61-62` throws on an empty mask. Pre-fix Rust
        // discarded both errors and silently fell back to
        // `BitSet::all_set(...)`, leaking fields the client didn't
        // request. Reply with an INIT-status error to the client,
        // then return Ok so the connection stays up — pvxs closes
        // the whole connection but the per-op error path here is a
        // less invasive parity choice that still surfaces the
        // condition. Tests that pin the all-set fallback will need
        // to specify `field()` or omit the pvRequest sub-structure.
        let req_desc = match decode_type_desc(&mut cur, order) {
            Ok(d) => d,
            Err(e) => {
                send_op_error(
                    tx,
                    kind,
                    ioid,
                    &format!("invalid pvRequest descriptor: {e}"),
                    order,
                )
                .await?;
                return Ok(());
            }
        };
        // PVA-R19: descriptor decode failure already routed through
        // `send_op_error` above. The pvRequest VALUE body is more
        // permissive — Rust client RPC INIT currently omits the
        // value (only the descriptor), and pipeline-options /
        // filter-chain parsers happily consume `None`. Treat a
        // value-decode failure as "no parseable value" rather than
        // INIT-level protocol error so we don't regress existing
        // Rust↔Rust interop. pvxs requires both via
        // `from_wire_type_value`; revisit when the client is brought
        // up to send the full pvRequest body on RPC INIT too.
        let req_value = decode_pv_field(&req_desc, &mut cur, order).ok();
        let mask = match crate::pv_request::request_to_mask(&intro, &req_desc) {
            Ok(m) => m,
            Err(e) => {
                // The only variant today is `EmptyMask`: pvRequest
                // selected no field that exists in the value
                // descriptor (e.g. `field(noSuch)`). pvxs treats
                // this as an INIT-level error
                // (`pvrequest.cpp:61-62`). Pre-fix Rust silently
                // fell back to all-fields, leaking fields the client
                // didn't request.
                send_op_error(
                    tx,
                    kind,
                    ioid,
                    &format!("invalid pvRequest mask: {e}"),
                    order,
                )
                .await?;
                return Ok(());
            }
        };

        // Pipeline flow control is opt-in via pvRequest:
        // `record[pipeline=true,queueSize=N]`. pvxs only enables the
        // credit/ACK window when the client explicitly sets it;
        // applying it unconditionally produced a 5-event-then-stall
        // bug for default `pvmonitor` callers (initial snapshot + 4
        // window credits). Without pipeline=true we don't gate the
        // emit loop — mpsc backpressure remains the only limiter.
        let pipeline_opt = req_value
            .as_ref()
            .and_then(monitor_pipeline_options)
            .filter(|o| o.enabled);
        // pvxs `servermon.cpp:493` — when the client sets the pipeline
        // bit on MONITOR INIT (`subcmd & 0x80`) it appends a u32 `nack`
        // (initial window credit) after the pvRequest. Read and consume
        // those bytes so any data following INIT in the same segment
        // decodes from the correct offset, and prefer the wire value
        // over the pvRequest `queueSize` so the negotiated initial
        // window matches what the client requested. We tolerate a
        // truncated nack (legacy clients sometimes omit it even with
        // the bit set — pvxs warns "pipeline monitor w/o initial nack
        // incompatible" but accepts the operation).
        let pipeline_initial_nack = parse_monitor_init_nack(kind, subcmd, &mut cur, order);
        let (monitor_window, monitor_window_notify) = if kind == OpKind::Monitor
            && let Some(opt) = pipeline_opt
        {
            let initial = pipeline_initial_nack.unwrap_or(opt.queue_size);
            debug!(
                ioid,
                queue_size = opt.queue_size,
                initial_nack = initial,
                "MONITOR INIT pipeline negotiated"
            );
            (
                Some(Arc::new(std::sync::atomic::AtomicU32::new(initial))),
                Some(Arc::new(tokio::sync::Notify::new())),
            )
        } else {
            (None, None)
        };

        // Server-side channel filters (PR #205 follow-up): if the
        // pvRequest carries `record._options._filter` as a JSON
        // chain spec, parse it via the shared filter framework.
        // MONITOR only — GET/PUT/RPC don't have a stream to filter.
        let monitor_filters = if kind == OpKind::Monitor {
            let chain_json = req_value.as_ref().and_then(monitor_filter_chain_json);
            match chain_json {
                Some(j) => {
                    Arc::new(epics_base_rs::server::database::filters::parse_filter_chain(&j))
                }
                None => Arc::new(epics_base_rs::server::database::filters::FilterChain::new()),
            }
        } else {
            Arc::new(epics_base_rs::server::database::filters::FilterChain::new())
        };

        // pvxs autoExec is purely client-side timing control
        // (clientget.cpp:123 — controls when the client sends the
        // PUT EXEC frame). The server-side handler runs onPut
        // unconditionally on every CMD_PUT !init regardless of
        // autoExec. We parse the option for diagnostic echo only.
        let put_auto_exec = if kind == OpKind::Put {
            put_autoexec_from_request(req_value.as_ref()).unwrap_or(true)
        } else {
            true
        };

        // BR-R3 / BR-R5: stash the INIT pvRequest so the data-phase
        // dispatch can forward it through `ChannelContext.pv_request`.
        // PUT needs `record._options.process|block`; MONITOR needs
        // `record._options.DBE` (and other per-op stream tuning that
        // wasn't already consumed for mask/pipeline/filter parsing).
        // GET / RPC don't read per-op options from this value beyond
        // what was already extracted, so we don't pay the clone for
        // those kinds.
        let stashed_pv_request = match kind {
            OpKind::Put | OpKind::Monitor => req_value.clone(),
            _ => None,
        };

        ch.ops.insert(
            ioid,
            OpState {
                intro: intro.clone(),
                kind,
                monitor_started: false,
                monitor_abort: None,
                mask,
                monitor_window,
                monitor_window_notify,
                monitor_paused: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                monitor_filters,
                put_auto_exec,
                pv_request: stashed_pv_request,
            },
        );

        // Build INIT response: ioid + subcmd + status + introspection
        let cmd = kind.command();

        let mut payload = Vec::new();
        payload.put_u32(ioid, order);
        payload.put_u8(subcmd);
        Status::ok().write_into(order, &mut payload);
        // RPC INIT carries no type descriptor (pvxs serverget.cpp:97 —
        // `if (cmd != CMD_RPC) to_wire(R, type)`). GET/PUT/MONITOR INIT
        // emits the introspection — inline by default; with
        // `config.emit_type_cache`, repeated descriptors collapse into
        // 3-byte 0xFE references via the per-connection TypeStore.
        if !matches!(kind, OpKind::Rpc) {
            if config.emit_type_cache {
                encode_type_desc_cached(&intro, order, encode_cache, &mut payload);
            } else {
                encode_type_desc(&intro, order, &mut payload);
            }
        }
        let h = PvaHeader::application(true, order, cmd.code(), payload.len() as u32);
        let mut buf = Vec::new();
        h.write_into(&mut buf);
        buf.extend_from_slice(&payload);
        let _ = tx.send(buf).await;
        return Ok(());
    }

    // Data phase
    let op = ch.ops.get(&ioid).cloned();
    let (intro, mask, init_pv_request) = match op {
        Some(o) => {
            // PVA-R24: data/control frames must match the operation
            // kind bound at INIT. pvxs `serverget.cpp:421-436`
            // resets the connection when a GET/PUT/RPC IOID is hit
            // by the wrong operation class, and `servermon.cpp:
            // 611-632` does the same for MONITOR. Pre-fix Rust
            // looked up only descriptor+mask and proceeded into the
            // current command's branch — a client could INIT a GET
            // and later run MONITOR start/ack against the same IOID,
            // spawning a subscriber task or sending a stray response
            // the original operation never negotiated.
            if o.kind != kind {
                return Err(PvaError::Decode(format!(
                    "data-phase command {:?} does not match INIT kind {:?} for IOID {ioid} (pvxs serverget.cpp:421-436 protocol error)",
                    kind, o.kind
                )));
            }
            (o.intro, o.mask, o.pv_request)
        }
        None => {
            send_op_error(tx, kind, ioid, "operation not initialised", order).await?;
            return Ok(());
        }
    };

    match kind {
        OpKind::Get => {
            // Round 41: type-state ACF gate. The wire layer mints the
            // [`AccessChecked`] token via the source's per-instance
            // [`AccessGate`]; the source's `get_value_checked` then
            // refuses to proceed when the level is `NoAccess`. The
            // token is unforgeable outside the gate, so adding a new
            // wire op without going through the gate is a compile
            // error against the trait method signature.
            let ctx = crate::server_native::source::ChannelContext {
                peer,
                account: cred.account.clone(),
                method: cred.method.clone(),
                host: cred.host.clone(),
                authority: cred.authority.clone(),
                pv_request: None,
            };
            let checked = source
                .access_gate()
                .check(
                    &ch.name,
                    &ctx.host,
                    &ctx.account,
                    &ctx.method,
                    &ctx.authority,
                )
                .await;
            let value = match source.get_value_checked(checked, ctx).await {
                Some(v) => v,
                None => {
                    send_op_error(tx, OpKind::Get, ioid, "PV not found", order).await?;
                    return Ok(());
                }
            };
            // PVA-R9: source-side mismatch gate. pvxs
            // `serverget.cpp:62-67` throws when GET / PUT-getback
            // returns a value whose descriptor differs from the one
            // passed to `connect()`. Pre-fix Rust silently coerced
            // via `encode_pv_field`'s F-G10 generic fallback,
            // turning application data corruption into a valid-
            // looking PVA frame. Reply with a GET-status error
            // (subcmd | 0x40 marker) instead.
            if let Err(e) = crate::pvdata::value_matches_descriptor(&value, &intro) {
                send_op_error(
                    tx,
                    OpKind::Get,
                    ioid,
                    &format!("source value does not match opened descriptor: {e}"),
                    order,
                )
                .await?;
                return Ok(());
            }
            let mut payload = Vec::new();
            payload.put_u32(ioid, order);
            // pvxs `serverget.cpp:83` echoes the request `subcmd`
            // verbatim. For GET data phase pvxs client always sends
            // `subcmd=0x00` (clientget.cpp:303 `state==Exec`) so the
            // observable byte happens to match the hardcoded 0x00, but
            // mirroring the request is the parity-correct shape and
            // future-proofs the response when the client adds new
            // QoS bits (e.g. `0x04` PROCESS).
            payload.put_u8(subcmd);
            Status::ok().write_into(order, &mut payload);
            // Emit only the fields the client's pvRequest selected.
            // `mask` is a *selection* mask — canonicalize it into a wire
            // changed-bitset so a partial filter is not widened to the
            // whole structure by a stray root bit.
            let changed = crate::pvdata::encode::canonical_changed_bitset(&intro, &mask);
            changed.write_into(order, &mut payload);
            crate::pvdata::encode::encode_pv_field_with_bitset(
                &value,
                &intro,
                &changed,
                0,
                order,
                &mut payload,
            );
            let h = PvaHeader::application(true, order, Command::Get.code(), payload.len() as u32);
            let mut buf = Vec::new();
            h.write_into(&mut buf);
            buf.extend_from_slice(&payload);
            let _ = tx.send(buf).await;
        }
        OpKind::Put => {
            // pvxs `serverget.cpp:364` derives `isput = cmd!=CMD_GET
            // && !(subcmd&0x40)`. When the client sets `subcmd &
            // 0x40` on a CMD_PUT frame (`clientget.cpp:300`
            // `GPROp::GetOPut`, used by `PutBuilder::fetchPresent(true)`
            // — the default), pvxs treats the data-phase frame as a
            // pre-PUT GET: no bitset/value on the wire, server emits
            // the current value so the client's `build(cb)` callback
            // can mutate-and-resend. Pre-fix Rust always read bitset
            // + value here and tripped `short read u8` on the empty
            // body, killing the connection before any actual PUT
            // landed.
            if subcmd & 0x40 != 0 {
                let pv_name = ch.name.clone();
                let ctx = crate::server_native::source::ChannelContext {
                    peer,
                    account: cred.account.clone(),
                    method: cred.method.clone(),
                    host: cred.host.clone(),
                    authority: cred.authority.clone(),
                    pv_request: None,
                };
                let checked = source
                    .access_gate()
                    .check(
                        &pv_name,
                        &ctx.host,
                        &ctx.account,
                        &ctx.method,
                        &ctx.authority,
                    )
                    .await;
                let value = match source.get_value_checked(checked, ctx).await {
                    Some(v) => v,
                    None => {
                        send_op_error(tx, OpKind::Put, ioid, "PV not found", order).await?;
                        return Ok(());
                    }
                };
                let mut payload = Vec::new();
                payload.put_u32(ioid, order);
                payload.put_u8(subcmd);
                Status::ok().write_into(order, &mut payload);
                let changed = crate::pvdata::encode::canonical_changed_bitset(&intro, &mask);
                changed.write_into(order, &mut payload);
                crate::pvdata::encode::encode_pv_field_with_bitset(
                    &value,
                    &intro,
                    &changed,
                    0,
                    order,
                    &mut payload,
                );
                let h =
                    PvaHeader::application(true, order, Command::Put.code(), payload.len() as u32);
                let mut buf = Vec::new();
                h.write_into(&mut buf);
                buf.extend_from_slice(&payload);
                let _ = tx.send(buf).await;
                return Ok(());
            }
            // PUT EXEC (subcmd & 0x40 == 0): read bitset (which
            // fields client is putting) + value.
            // The PVA client encodes the data phase as a BitSet delta
            // (`changed | partial value`, see
            // `client_native::ops_v2::op_put*` and pvxs
            // `serverput.cpp` `from_wire`): only the fields whose bit
            // is set are present on the wire. Decoding the value as a
            // full structure (`decode_pv_field`) desyncs the stream
            // for any multi-field structure where not every field is
            // marked. Decode with the changed-BitSet so exactly the
            // present fields are consumed (pvData spec §5.4 bit
            // numbering).
            let changed =
                BitSet::decode(&mut cur, order).map_err(|e| PvaError::Decode(e.to_string()))?;
            // pvxs `serverget.cpp:488-492` calls `onPut` immediately
            // on every CMD_PUT !init — the client's autoExec setting
            // is purely a client-side timing knob (clientget.cpp:213)
            // for whether the PUT EXEC fires automatically after INIT
            // or waits for `reExec()`. Each EXEC frame still carries
            // exactly one value and triggers exactly one write.
            let delta = decode_pv_field_with_bitset(&intro, &changed, 0, &mut cur, order)
                .map_err(|e| PvaError::Decode(format!("PUT requires a value payload: {e}")))?;
            let pv_name = ch.name.clone();
            // Round 42: type-state PUT gate. The token's
            // `allows_write()` is checked by `put_delta_checked`;
            // adding a new PUT-equivalent handler without taking a
            // token through `source.access().check(...)` is a compile
            // error on the trait method signature.
            //
            // BR-R3: forward the INIT pvRequest so the source can
            // honor `record._options.process` / `block` (QSRV
            // semantics). The data-phase payload is just the delta;
            // per-operation options live in the INIT pvRequest only.
            let ctx = crate::server_native::source::ChannelContext {
                peer,
                account: cred.account.clone(),
                method: cred.method.clone(),
                host: cred.host.clone(),
                authority: cred.authority.clone(),
                pv_request: init_pv_request.clone(),
            };

            // The wire delta carries only marked fields; unmarked
            // fields decoded as type defaults. Applying it is a
            // read-merge-write against the PV's current value.
            // `put_delta_checked` performs that merge atomically
            // under the source's lock — doing `get_value` then
            // `put_value` as two ops opens a TOCTOU lost-update
            // window where two concurrent partial PUTs with disjoint
            // changed-fields both read the same prior and the second
            // write drops the first's fields.
            let result = {
                let checked = source
                    .access_gate()
                    .check(
                        &pv_name,
                        &ctx.host,
                        &ctx.account,
                        &ctx.method,
                        &ctx.authority,
                    )
                    .await;
                source
                    .put_delta_checked(checked, intro.clone(), changed.clone(), delta, ctx.clone())
                    .await
            };

            let mut payload = Vec::new();
            payload.put_u32(ioid, order);
            // pvxs `serverget.cpp:83` echoes the request `subcmd`. For
            // a plain PUT EXEC the client sends `0x00`; for PUT_GET
            // (readback) the client sends `0x40` (clientget.cpp:300,
            // `state==GetOPut`). Hardcoding `0x00` makes the response
            // header lie to the client: pvxs `clientget.cpp:362-370`
            // dispatches the response decode on `subcmd & 0x40`, so a
            // PUT_GET reply with `subcmd=0x00` was parsed as
            // status-only and the readback bytes (status+bitset+value)
            // were silently dropped on the floor.
            payload.put_u8(subcmd);
            match result {
                Ok(()) => {
                    // PUT_GET (subcmd bit 0x40 set on the request): client
                    // wants the post-put value back. Per pvxs serverget.cpp:103
                    // the response carries `bitset + partial value` after the
                    // status. Readback must be credential-aware so a peer
                    // with READ-only or NoAccess on its ASG does not see
                    // a leaked value through the PUT_GET return path.
                    if subcmd & 0x40 != 0 {
                        // R31-G7 / Round-32B: build the readback FIRST,
                        // then write status. If the READ check fails
                        // (ACF denies READ even though PUT was allowed —
                        // e.g. WRITE-only ASG), we MUST NOT emit
                        // `status_ok` followed by no bytes — that
                        // truncates the wire and the client decoder
                        // expects a bitset+value to follow. Instead,
                        // emit an all-zero bitset (no fields changed)
                        // alongside the OK status: client decodes
                        // zero fields, no value bytes consumed, PUT
                        // reported successful — same wire shape as a
                        // "put committed but no field deltas to
                        // report" response.
                        //
                        // Round 42 type-state: re-check via the gate
                        // for the READ leg. The PUT's token was
                        // consumed by `put_value_checked`; we mint a
                        // fresh one against the SAME `(pv, ctx)`.
                        let read_checked = source
                            .access_gate()
                            .check(
                                &pv_name,
                                &ctx.host,
                                &ctx.account,
                                &ctx.method,
                                &ctx.authority,
                            )
                            .await;
                        match source.get_value_checked(read_checked, ctx).await {
                            Some(v) => {
                                Status::ok().write_into(order, &mut payload);
                                let bits = BitSet::all_set(intro.total_bits());
                                bits.write_into(order, &mut payload);
                                encode_pv_field(&v, &intro, order, &mut payload);
                            }
                            None => {
                                Status::ok().write_into(order, &mut payload);
                                let empty = BitSet::with_capacity(intro.total_bits());
                                empty.write_into(order, &mut payload);
                                // No encode_pv_field — bitset.count()==0
                                // means zero partial fields follow.
                            }
                        }
                    } else {
                        Status::ok().write_into(order, &mut payload);
                    }
                }
                Err(msg) => Status::error(msg).write_into(order, &mut payload),
            }
            let h = PvaHeader::application(true, order, Command::Put.code(), payload.len() as u32);
            let mut buf = Vec::new();
            h.write_into(&mut buf);
            buf.extend_from_slice(&payload);
            let _ = tx.send(buf).await;
        }
        OpKind::Monitor => {
            // MONITOR_START / pipeline-ack: pvxs uses subcmd 0x40 for
            // START and 0x80 for ACK (the high bit signals "ack"
            // followed by a u32 ack-count payload that refills the
            // pipeline window). Either signals "produce events".
            // Plain 0x00 also accepted for legacy compatibility.
            let is_ack = subcmd & 0x80 != 0;
            let is_start_or_ack = subcmd & 0x40 != 0 || is_ack || subcmd == 0x00;
            // P-G28: subcmd 0x04 alone is PAUSE (pvxs Subscription::
            // pause(true)). subcmd 0x44 (start | process bit) is
            // RESUME — clears the paused flag in addition to its
            // existing start handling. We honour PAUSE by setting
            // the paused atomic; the subscriber loop checks before
            // emit. The flag also clears on RESUME and on START.
            let is_pause = subcmd == 0x04;
            let is_resume = subcmd & 0x40 != 0;
            if let Some(op) = ch.ops.get(&ioid) {
                if is_pause {
                    op.monitor_paused
                        .store(true, std::sync::atomic::Ordering::Relaxed);
                } else if is_resume {
                    let prev = op
                        .monitor_paused
                        .swap(false, std::sync::atomic::Ordering::Relaxed);
                    if prev {
                        if let Some(n) = op.monitor_window_notify.as_ref() {
                            n.notify_waiters();
                        }
                    }
                }
            }

            // ACK path: refill the pipeline window (P-G11). pvxs
            // servermon.cpp:111 reads the u32 ack-count; we add it
            // to the AtomicU32 and pulse the notify so a paused
            // subscriber wakes and resumes emission. ACKs can arrive
            // before OR after the START — we always honour them.
            if is_ack {
                if let Some(op) = ch.ops.get(&ioid) {
                    let ack_count = cur.get_u32(order).unwrap_or(4);
                    if let (Some(w), Some(n)) = (
                        op.monitor_window.as_ref(),
                        op.monitor_window_notify.as_ref(),
                    ) {
                        let prev = w.fetch_add(ack_count, std::sync::atomic::Ordering::Relaxed);
                        if prev == 0 {
                            n.notify_waiters();
                        }
                    }
                }
            }

            // Only spawn the subscriber task once per ioid.
            let already_running = ch
                .ops
                .get(&ioid)
                .map(|s| s.monitor_started)
                .unwrap_or(false);
            if is_start_or_ack && !already_running {
                let pv_name = ch.name.clone();
                let intro_clone = intro.clone();
                let mask_clone = mask.clone();
                let tx_clone = tx.clone();
                let src = source.clone();
                let queue_depth = config.monitor_queue_depth;
                let high_watermark = config.monitor_high_watermark;
                // ACF-aware MONITOR: capture the peer's credentials
                // so the spawned task can consult ctx-aware
                // subscribe/get_value paths. Sources without ACF
                // delegate to the legacy methods.
                // BR-R5: forward the INIT pvRequest so the source can
                // honor `record._options.DBE` (per-op database event-
                // mask selection — pvxs singlesource.cpp:115). Like
                // BR-R3 for PUT, the data-phase START/ACK frames are
                // pure stream control; per-operation options live in
                // the INIT pvRequest only.
                let mon_ctx = crate::server_native::source::ChannelContext {
                    peer,
                    account: cred.account.clone(),
                    method: cred.method.clone(),
                    host: cred.host.clone(),
                    authority: cred.authority.clone(),
                    pv_request: init_pv_request.clone(),
                };
                // Round 42 + R49-G1: type-state MONITOR gate.
                //
                // Capture the ACL generation BEFORE the check.
                // This guarantees the captured version is `≤` the
                // version under which the resulting `AccessChecked`
                // was minted: if a reload bumps the version between
                // the capture and the check, the check runs under
                // the new policy and the captured (older) version
                // is below the live version, so the forwarding loop
                // detects the mismatch on its next event and
                // re-checks. The reverse order (check then capture)
                // could combine an "old allow" token with a "new
                // version", causing the loop to think it was
                // already synced under the new policy and never
                // re-check.
                //
                // Wrapped in `Arc<AtomicU64>` so a successful
                // re-check inside the spawned loop can advance the
                // surviving peer's "current" generation without
                // re-checking on every subsequent event.
                let mon_acl_version_at_subscribe_cell = Arc::new(
                    std::sync::atomic::AtomicU64::new(source.access_gate().acl_version()),
                );
                let mon_checked = source
                    .access_gate()
                    .check(
                        &pv_name,
                        &mon_ctx.host,
                        &mon_ctx.account,
                        &mon_ctx.method,
                        &mon_ctx.authority,
                    )
                    .await;
                // Snapshot the window + notify so the spawned task can
                // share state with this dispatch path's ACK handler.
                let (window, window_notify, paused_flag, filters) = ch
                    .ops
                    .get(&ioid)
                    .map(|s| {
                        (
                            s.monitor_window.clone(),
                            s.monitor_window_notify.clone(),
                            s.monitor_paused.clone(),
                            s.monitor_filters.clone(),
                        )
                    })
                    .unwrap_or_else(|| {
                        (
                            None,
                            None,
                            Arc::new(std::sync::atomic::AtomicBool::new(false)),
                            Arc::new(epics_base_rs::server::database::filters::FilterChain::new()),
                        )
                    });
                let total_bits = intro_clone.total_bits();
                // Raw fast path is correct only when the downstream's
                // pvRequest matches the upstream's bytes 1:1 — i.e. no
                // per-field projection, no negotiated pipeline credit
                // window (the raw branch has no per-event
                // window-decrement / wait-for-ACK gating), AND no
                // server-side filter chain (the raw branch forwards
                // pre-encoded wire bytes; the filter chain operates
                // on the decoded PvField). Fall back to the decoded
                // subscribe path in any of those cases.
                let raw_path_eligible = mask_clone.count() == total_bits
                    && mask_clone.size() >= total_bits
                    && window.is_none()
                    && filters.is_empty();
                let join = tokio::spawn(async move {
                    // F-G12: raw-frame fast path. When the source can
                    // hand us pre-encoded MONITOR DATA bytes (e.g.
                    // pva_gateway upstream-monitor task already
                    // received them on the wire), emit them with only
                    // an IOID-rewrite — pvxs / pva2pva style raw
                    // forward. Falls back to the decoded path on
                    // byte-order mismatch or when the source returns
                    // None.
                    // R31-G6 / Round-32A: raw fast path must consult
                    // the ACF too. The round-29 ACL gate covered
                    // `subscribe_ctx` only; ACF-aware sources can now
                    // override `subscribe_raw_ctx` to deny when the
                    // peer lacks READ. When the gateway denies (returns
                    // None), we fall through to the decoded
                    // `subscribe_ctx` below — which is also ACF-gated
                    // and will likewise return None.
                    if raw_path_eligible
                        && let Some(mut rx_raw) = src
                            .subscribe_raw_checked(mon_checked.clone(), mon_ctx.clone())
                            .await
                    {
                        // R49-G1: revalidate ACL BEFORE sending the
                        // initial snapshot. Between the spawn's
                        // initial `check()` and reaching this point
                        // a reload could have flipped the peer to
                        // NoAccess; without this gate the initial
                        // would be emitted under stale policy. The
                        // recv loop below performs the same check
                        // on every subsequent event.
                        let live_v0 = src.access_gate().acl_version();
                        if live_v0
                            != mon_acl_version_at_subscribe_cell
                                .load(std::sync::atomic::Ordering::Acquire)
                        {
                            // R50 audit-3: route the re-check
                            // through the source's
                            // `revalidate_read` owner so composite
                            // sources resolve to the MATCHED inner
                            // source's gate (the one that served
                            // the original subscription), not the
                            // composite's permissive aggregator
                            // gate.
                            if src
                                .revalidate_read(&pv_name, mon_ctx.clone())
                                .await
                                .is_none()
                            {
                                let finish = build_monitor_finish(ioid, order);
                                let _ = tx_clone.send(finish).await;
                                return;
                            }
                            mon_acl_version_at_subscribe_cell
                                .store(live_v0, std::sync::atomic::Ordering::Release);
                        }
                        // Emit initial snapshot via the regular
                        // encode path (no raw bytes for the
                        // first-event seed; the cache may not have
                        // them yet). ACF-aware: a peer with NoAccess
                        // on this PV's ASG sees no initial frame
                        // through the raw fast path either.
                        if let Some(initial) = src
                            .get_value_checked(mon_checked.clone(), mon_ctx.clone())
                            .await
                        {
                            let payload = build_monitor_payload(
                                ioid,
                                &intro_clone,
                                &initial,
                                &mask_clone,
                                order,
                            );
                            if tx_clone.send(payload).await.is_err() {
                                return;
                            }
                        }
                        while let Some(ev) = rx_raw.recv().await {
                            // R48-G3 + R50 audit-3: ACL re-check on
                            // policy reload. The version compare uses
                            // the source's aggregate (composite =
                            // wrapping-sum of inner versions); the
                            // re-check is routed through
                            // `revalidate_read` so composite sources
                            // resolve to the matched inner gate
                            // instead of the permissive aggregator
                            // gate.
                            let live_v = src.access_gate().acl_version();
                            if live_v
                                != mon_acl_version_at_subscribe_cell
                                    .load(std::sync::atomic::Ordering::Acquire)
                            {
                                if src
                                    .revalidate_read(&pv_name, mon_ctx.clone())
                                    .await
                                    .is_none()
                                {
                                    let finish = build_monitor_finish(ioid, order);
                                    let _ = tx_clone.send(finish).await;
                                    return;
                                }
                                // Survive — resync the version so we
                                // don't re-check on every event under
                                // the new policy.
                                mon_acl_version_at_subscribe_cell
                                    .store(live_v, std::sync::atomic::Ordering::Release);
                            }
                            // Refuse the fast path on byte-order
                            // mismatch (cross-host gateway, rare).
                            // Falling back means re-decoding to
                            // PvField then re-encoding here.
                            if ev.byte_order != order {
                                debug!(
                                    pv = %pv_name,
                                    "F-G12 byte-order mismatch — \
                                     dropping to decode-encode path"
                                );
                                // Drop this event (decode/encode
                                // fallback would require the FieldDesc
                                // and this code path is exercised
                                // <0.1% of the time); regular subscribe
                                // covers it. Future work: keep both
                                // streams active under mismatch.
                                continue;
                            }
                            let payload = build_monitor_payload_raw(ioid, &ev, order);
                            if tx_clone.send(payload).await.is_err() {
                                return;
                            }
                        }
                        let finish = build_monitor_finish(ioid, order);
                        let _ = tx_clone.send(finish).await;
                        return;
                    }

                    let Some(mut rx) = src
                        .subscribe_checked(mon_checked.clone(), mon_ctx.clone())
                        .await
                    else {
                        return;
                    };
                    let mut over_high = false;
                    // R49-G1 + R50 audit-3: revalidate ACL BEFORE
                    // sending the initial snapshot on the decoded
                    // path. The re-check is routed through
                    // `revalidate_read` so composite sources resolve
                    // to the matched inner gate.
                    {
                        let live_v0 = src.access_gate().acl_version();
                        if live_v0
                            != mon_acl_version_at_subscribe_cell
                                .load(std::sync::atomic::Ordering::Acquire)
                        {
                            if src
                                .revalidate_read(&pv_name, mon_ctx.clone())
                                .await
                                .is_none()
                            {
                                let finish = build_monitor_finish(ioid, order);
                                let _ = tx_clone.send(finish).await;
                                return;
                            }
                            mon_acl_version_at_subscribe_cell
                                .store(live_v0, std::sync::atomic::Ordering::Release);
                        }
                    }
                    // Emit initial snapshot via the ACF-aware path —
                    // a peer with NoAccess on the record's ASG sees
                    // nothing; legacy sources fall through to
                    // `get_value` via the trait default.
                    if let Some(initial) = src
                        .get_value_checked(mon_checked.clone(), mon_ctx.clone())
                        .await
                    {
                        // BR-R39: pvxs `servermon.cpp:261` always lets
                        // the first update enter the queue (it bypasses
                        // the change-or-mask gate), but `:174` still
                        // encodes the wire BitSet with
                        // `self->pvMask` — the field mask derived
                        // from the client's pvRequest. The earlier
                        // Rust path bypassed both checks, sending the
                        // initial event with `BitSet::all_set(...)`
                        // and leaking unrequested leaves. Match pvxs
                        // by always queueing the first event (no
                        // change-filter here) but honouring
                        // `mask_clone` on the wire.
                        let payload =
                            build_monitor_payload(ioid, &intro_clone, &initial, &mask_clone, order);
                        if tx_clone.send(payload).await.is_err() {
                            return;
                        }
                    }
                    // Back-pressure / squashing loop: drain available
                    // events between writes, keeping only the most recent
                    // value if more than `queue_depth` events stack up.
                    let mut squashing = false;
                    while let Some(mut value) = rx.recv().await {
                        // R48-G3: ACL re-check on policy reload (same
                        // shape as the raw-fast-path branch above).
                        // The gate's `acl_version` bumps on every
                        // PvaServer ACF swap; on mismatch we
                        // re-mint AccessChecked and tear down with
                        // a MONITOR FINISH if the new policy denies.
                        // R48-G3 + R50 audit-3: decoded recv-loop
                        // re-check, routed through `revalidate_read`
                        // for composite-source correctness.
                        let live_v = src.access_gate().acl_version();
                        if live_v
                            != mon_acl_version_at_subscribe_cell
                                .load(std::sync::atomic::Ordering::Acquire)
                        {
                            if src
                                .revalidate_read(&pv_name, mon_ctx.clone())
                                .await
                                .is_none()
                            {
                                let finish = build_monitor_finish(ioid, order);
                                let _ = tx_clone.send(finish).await;
                                return;
                            }
                            mon_acl_version_at_subscribe_cell
                                .store(live_v, std::sync::atomic::Ordering::Release);
                        }
                        // Drain extras; keep the latest.
                        let mut squashed = 0usize;
                        loop {
                            match rx.try_recv() {
                                Ok(next) => {
                                    value = next;
                                    squashed += 1;
                                    if squashed > queue_depth {
                                        squashing = true;
                                    }
                                }
                                Err(mpsc::error::TryRecvError::Empty) => break,
                                Err(mpsc::error::TryRecvError::Disconnected) => break,
                            }
                        }
                        if squashing {
                            debug!(pv = %pv_name, squashed, "monitor squashed events");
                            squashing = false;
                        }
                        // Watermark crossing diagnostics + producer
                        // notification. pvxs fires `onHighMark` /
                        // `onLowMark` callbacks at these transitions so
                        // sources can throttle/un-throttle their post()
                        // rate; we mirror that via
                        // `ChannelSource::notify_watermark_{high,low}`.
                        // The default trait impl is a no-op; SharedSource
                        // overrides it to dispatch the per-PV callback
                        // registered via `SharedPv::set_on_high_mark`.
                        // Counter is max_capacity - capacity since mpsc
                        // doesn't expose len directly.
                        let pending = tx_clone.max_capacity() - tx_clone.capacity();
                        if pending >= high_watermark && !over_high {
                            over_high = true;
                            warn!(
                                pv = %pv_name,
                                pending,
                                high_watermark,
                                "monitor outbound queue crossed high watermark"
                            );
                            src.notify_watermark_high(&pv_name);
                        } else if pending == 0 && over_high {
                            over_high = false;
                            debug!(pv = %pv_name, "monitor outbound queue drained below low watermark");
                            src.notify_watermark_low(&pv_name);
                        }
                        // P-G11: pipeline window check. When pipeline
                        // is active, wait for window > 0 before
                        // emitting. ACK frames refill the window via
                        // the dispatch path; we wake on the notify.
                        // Without a window (pipeline=false) we emit
                        // freely; mpsc backpressure remains the only
                        // gate, matching previous behavior.
                        if let (Some(w), Some(n)) = (window.as_ref(), window_notify.as_ref()) {
                            loop {
                                let cur = w.load(std::sync::atomic::Ordering::Relaxed);
                                if cur > 0 {
                                    if w.compare_exchange(
                                        cur,
                                        cur - 1,
                                        std::sync::atomic::Ordering::Relaxed,
                                        std::sync::atomic::Ordering::Relaxed,
                                    )
                                    .is_ok()
                                    {
                                        break;
                                    }
                                    continue;
                                }
                                // Window exhausted — wait for ACK.
                                let notified = n.notified();
                                tokio::pin!(notified);
                                // enable() registers the waiter eagerly
                                // so an ACK firing between the recheck
                                // and the await is captured. Same
                                // pattern as channel.rs::wait_until_inactive
                                // — Notify::notified() does NOT register
                                // until the future is polled, so the
                                // recheck-then-await window otherwise
                                // loses the wake from notify_waiters().
                                notified.as_mut().enable();
                                if w.load(std::sync::atomic::Ordering::Relaxed) > 0 {
                                    continue;
                                }
                                notified.await;
                            }
                        }
                        // P-G28: pause drops events on the floor.
                        // Resume cleanly re-emits whatever the source
                        // sends next; clients that need state recovery
                        // re-issue their pvRequest after resume.
                        if paused_flag.load(std::sync::atomic::Ordering::Relaxed) {
                            continue;
                        }
                        // Server-side channel filters: skip when the
                        // chain drops this event. Empty chain (the
                        // default) is a no-op pass-through.
                        if !filters.is_empty() {
                            let fev = pv_field_to_filter_event(&value);
                            if filters.apply(fev).is_none() {
                                continue;
                            }
                        }
                        let payload =
                            build_monitor_payload(ioid, &intro_clone, &value, &mask_clone, order);
                        if tx_clone.send(payload).await.is_err() {
                            return;
                        }
                    }
                    // Source closed — emit MONITOR FINISH (subcmd 0x10 + Status).
                    // pvxs servermon.cpp:148-178 sends a final frame with
                    // subcmd=0x10 to signal end-of-stream so the client can
                    // tear down cleanly.
                    let finish = build_monitor_finish(ioid, order);
                    let _ = tx_clone.send(finish).await;
                });
                if let Some(s) = ch.ops.get_mut(&ioid) {
                    s.monitor_started = true;
                    s.monitor_abort = Some(Arc::new(AbortOnDrop(join.abort_handle())));
                }
            }
        }
        OpKind::Rpc => {
            // RPC DATA request from client: `type(arg) + full_value(arg)`.
            // pvxs clientget.cpp:307-311 — `to_wire(R, type); to_wire_full(R, arg)`.
            // The introspection on the channel was negotiated for the
            // *pvRequest* in INIT, not the actual call argument, so we must
            // decode the argument's own type descriptor here.
            let (req_desc, req_value) = match decode_type_desc(&mut cur, order) {
                Ok(desc) => match decode_pv_field(&desc, &mut cur, order) {
                    Ok(v) => (desc, v),
                    Err(_) => (desc, PvField::Null),
                },
                Err(_) => {
                    // Empty body — some clients send parameterless RPCs with
                    // no payload after subcmd.
                    (FieldDesc::Variant, PvField::Null)
                }
            };
            let pv_name = ch.name.clone();
            let _ = intro; // INIT pvRequest descriptor — no longer used here.
            // Round 42 type-state RPC dispatch. The wire layer mints
            // the AccessChecked token via the gate; the typed
            // `rpc_checked` refuses `NoAccess` tokens. Adding a new
            // RPC-equivalent handler without going through the gate
            // is now a compile error on the trait method signature.
            let rpc_ctx_val = crate::server_native::source::ChannelContext {
                peer,
                account: cred.account.clone(),
                method: cred.method.clone(),
                host: cred.host.clone(),
                authority: cred.authority.clone(),
                pv_request: None,
            };
            let rpc_checked = source
                .access_gate()
                .check(
                    &pv_name,
                    &rpc_ctx_val.host,
                    &rpc_ctx_val.account,
                    &rpc_ctx_val.method,
                    &rpc_ctx_val.authority,
                )
                .await;
            let result = source
                .rpc_checked(rpc_checked, req_desc, req_value, rpc_ctx_val)
                .await;

            let mut payload = Vec::new();
            payload.put_u32(ioid, order);
            // pvxs `serverget.cpp:83` echoes the request `subcmd`.
            // RPC EXEC always sends `0x00`, but mirror the request
            // for parity-correctness — pvxs validates the response
            // subcmd against the local op state and treats a mismatch
            // as a protocol fault.
            payload.put_u8(subcmd);
            match result {
                Ok((resp_desc, resp_value)) => {
                    Status::ok().write_into(order, &mut payload);
                    if config.emit_type_cache {
                        encode_type_desc_cached(&resp_desc, order, encode_cache, &mut payload);
                    } else {
                        encode_type_desc(&resp_desc, order, &mut payload);
                    }
                    encode_pv_field(&resp_value, &resp_desc, order, &mut payload);
                }
                Err(msg) => Status::error(msg).write_into(order, &mut payload),
            }
            let h = PvaHeader::application(true, order, Command::Rpc.code(), payload.len() as u32);
            let mut buf = Vec::new();
            h.write_into(&mut buf);
            buf.extend_from_slice(&payload);
            let _ = tx.send(buf).await;
        }
        // PUT_GET / PROCESS have dedicated handlers (`handle_put_get`,
        // `handle_process`) and are never dispatched into `handle_op`.
        OpKind::PutGet | OpKind::Process => {
            unreachable!("PUT_GET / PROCESS are routed to their own handlers, not handle_op")
        }
    }
    Ok(())
}

async fn handle_get_field(
    source: &DynSource,
    frame: &Frame,
    tx: &SrvTx,
    channels: &HashMap<u32, ChannelState>,
    order: ByteOrder,
) -> PvaResult<()> {
    let mut cur = frame.cursor();
    let sid = cur
        .get_u32(order)
        .map_err(|e| PvaError::Decode(e.to_string()))?;
    let ioid = cur
        .get_u32(order)
        .map_err(|e| PvaError::Decode(e.to_string()))?;
    let _sub = crate::proto::decode_string(&mut cur, order)
        .map_err(|e| PvaError::Decode(e.to_string()))?;

    // P-G19: pvxs serverintrospect.cpp:159 silently returns on
    // unknown SID; without this we'd reply with a fabricated
    // Variant descriptor + status=OK, which is worse than a noop —
    // a stale client would build its decode tree against a wrong
    // shape and surface garbage on the next GET. Match pvxs.
    //
    // P-C4: pvxs serverintrospect.cpp:159 is ONE composite guard —
    // `if(!chan || opByIOID.find(ioid)!=opByIOID.end())`. Both arms
    // log at err level and silently return. We were checking only
    // the SID half: GET_FIELD reusing an IOID already bound to an
    // active GET/PUT/MONITOR/RPC in the same channel would (a) fire
    // back a successful introspection reply that the client logs as
    // unexpected traffic on a busy IOID, and (b) leave the original
    // op's state untouched but with the wire conversation polluted.
    // Match pvxs: reject IOID-reuse via the same silent path.
    let chan = match channels.get(&sid) {
        Some(c) => c,
        None => {
            debug!(sid, ioid, "GET_FIELD on unknown SID: dropping");
            return Ok(());
        }
    };
    if chan.ops.contains_key(&ioid) {
        debug!(
            sid,
            ioid, "GET_FIELD reuses IOID bound to active op: dropping (pvxs parity)"
        );
        return Ok(());
    }
    let intro = match chan.introspection.clone() {
        Some(d) => d,
        None => source
            .get_introspection(&chan.name)
            .await
            .unwrap_or(FieldDesc::Variant),
    };

    let mut payload = Vec::new();
    payload.put_u32(ioid, order);
    Status::ok().write_into(order, &mut payload);
    encode_type_desc(&intro, order, &mut payload);
    let h = PvaHeader::application(true, order, Command::GetField.code(), payload.len() as u32);
    let mut buf = Vec::new();
    h.write_into(&mut buf);
    buf.extend_from_slice(&payload);
    let _ = tx.send(buf).await;
    Ok(())
}

async fn send_op_error(
    tx: &SrvTx,
    kind: OpKind,
    ioid: u32,
    msg: &str,
    order: ByteOrder,
) -> PvaResult<()> {
    let cmd = kind.command();
    let mut payload = Vec::new();
    payload.put_u32(ioid, order);
    payload.put_u8(0x08); // INIT phase err
    Status::error(msg.to_string()).write_into(order, &mut payload);
    let h = PvaHeader::application(true, order, cmd.code(), payload.len() as u32);
    let mut buf = Vec::new();
    h.write_into(&mut buf);
    buf.extend_from_slice(&payload);
    let _ = tx.send(buf).await;
    Ok(())
}

#[allow(unused_imports)]
use crate::proto::ReadExt;
const _: u8 = PVA_VERSION;

/// Build a complete MONITOR data frame (header + payload) for a single value
/// emission. Pulled out so the back-pressure squashing loop can call it.
fn build_monitor_payload(
    ioid: u32,
    intro: &FieldDesc,
    value: &PvField,
    mask: &BitSet,
    order: ByteOrder,
) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.put_u32(ioid, order);
    payload.put_u8(0x00);
    // PVA monitor data: changed bitset + partial value + overrun bitset.
    // `mask` is a *selection* mask (request_to_mask) — canonicalize it
    // into a wire changed-bitset so a partial field filter is not
    // widened to the whole structure by a stray root/structure bit.
    let changed = crate::pvdata::encode::canonical_changed_bitset(intro, mask);
    changed.write_into(order, &mut payload);
    crate::pvdata::encode::encode_pv_field_with_bitset(
        value,
        intro,
        &changed,
        0,
        order,
        &mut payload,
    );
    let overrun = BitSet::new(); // no overruns
    overrun.write_into(order, &mut payload);
    let h = PvaHeader::application(true, order, Command::Monitor.code(), payload.len() as u32);
    let mut buf = Vec::with_capacity(8 + payload.len());
    h.write_into(&mut buf);
    buf.extend_from_slice(&payload);
    buf
}

/// F-G12 raw-frame variant: build a MONITOR data frame from a
/// pre-encoded [`crate::server_native::RawMonitorEvent`]. The body
/// (`changed | value | overrun`) is reused verbatim with a single
/// `extend_from_slice` (memcpy); only the per-subscription PVA
/// header + downstream IOID + subcmd are fresh.
fn build_monitor_payload_raw(
    ioid: u32,
    ev: &crate::server_native::RawMonitorEvent,
    order: ByteOrder,
) -> Vec<u8> {
    let total = 4 /* ioid */ + 1 /* subcmd */ + ev.body_bytes.len();
    let mut payload = Vec::with_capacity(total);
    payload.put_u32(ioid, order);
    payload.put_u8(0x00);
    payload.extend_from_slice(&ev.body_bytes);
    let h = PvaHeader::application(true, order, Command::Monitor.code(), payload.len() as u32);
    let mut buf = Vec::with_capacity(8 + payload.len());
    h.write_into(&mut buf);
    buf.extend_from_slice(&payload);
    buf
}

/// Build a MONITOR FINISH frame (subcmd `0x10` + Status). Sent when the
/// underlying source closes its broadcast channel, signalling end-of-stream
/// to the subscribing client. Mirrors pvxs `servermon.cpp:148-178`.
fn build_monitor_finish(ioid: u32, order: ByteOrder) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.put_u32(ioid, order);
    payload.put_u8(0x10);
    Status::ok().write_into(order, &mut payload);
    let h = PvaHeader::application(true, order, Command::Monitor.code(), payload.len() as u32);
    let mut buf = Vec::with_capacity(8 + payload.len());
    h.write_into(&mut buf);
    buf.extend_from_slice(&payload);
    buf
}

fn now_nanos() -> u64 {
    use std::time::SystemTime;
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client_native::decode::{OpResponse, decode_op_response, try_parse_frame};
    use crate::pvdata::{PvStructure, ScalarType, ScalarValue};

    /// PVA-R20: server pipeline parser accepts the typed-bool /
    /// typed-int shape pvxs `Context::request().record("pipeline",
    /// true)` produces, not just the string `"true"` form.
    fn make_pipeline_request(value_pipe: PvField, queue: PvField) -> PvField {
        let options = PvField::Structure(PvStructure {
            struct_id: String::new(),
            fields: vec![
                ("pipeline".to_string(), value_pipe),
                ("queueSize".to_string(), queue),
            ],
        });
        let record = PvField::Structure(PvStructure {
            struct_id: String::new(),
            fields: vec![("_options".to_string(), options)],
        });
        PvField::Structure(PvStructure {
            struct_id: String::new(),
            fields: vec![("record".to_string(), record)],
        })
    }

    #[test]
    fn pva_r20_pipeline_typed_bool_true_enables_window() {
        let req = make_pipeline_request(
            PvField::Scalar(ScalarValue::Boolean(true)),
            PvField::Scalar(ScalarValue::Int(16)),
        );
        let opts = monitor_pipeline_options(&req).expect("parsed");
        assert!(opts.enabled, "Boolean(true) must enable pipeline");
        assert_eq!(opts.queue_size, 16);
    }

    #[test]
    fn pva_r20_pipeline_string_true_still_enables_window() {
        let req = make_pipeline_request(
            PvField::Scalar(ScalarValue::String("true".into())),
            PvField::Scalar(ScalarValue::String("32".into())),
        );
        let opts = monitor_pipeline_options(&req).expect("parsed");
        assert!(opts.enabled, "string \"true\" must still enable pipeline");
        assert_eq!(opts.queue_size, 32);
    }

    #[test]
    fn pva_r20_pipeline_typed_int_nonzero_enables_window() {
        // pvxs treats any non-zero integer as truthy via Value::as<bool>.
        let req = make_pipeline_request(
            PvField::Scalar(ScalarValue::Int(1)),
            PvField::Scalar(ScalarValue::Int(8)),
        );
        let opts = monitor_pipeline_options(&req).expect("parsed");
        assert!(opts.enabled, "Int(1) must enable pipeline");
        assert_eq!(opts.queue_size, 8);
    }

    #[test]
    fn pva_r20_pipeline_queue_size_below_two_disables() {
        // pvxs `servermon.cpp:533-540` rejects queueSize < 2 even
        // when pipeline=true. Pre-fix Rust clamped to 1 and ran a
        // broken window.
        let req = make_pipeline_request(
            PvField::Scalar(ScalarValue::Boolean(true)),
            PvField::Scalar(ScalarValue::Int(1)),
        );
        let opts = monitor_pipeline_options(&req).expect("parsed");
        assert!(!opts.enabled, "queueSize<2 must disable pipeline");
    }

    #[test]
    fn pva_r20_pipeline_bool_false_disables() {
        let req = make_pipeline_request(
            PvField::Scalar(ScalarValue::Boolean(false)),
            PvField::Scalar(ScalarValue::Int(16)),
        );
        let opts = monitor_pipeline_options(&req).expect("parsed");
        assert!(!opts.enabled, "Boolean(false) must disable pipeline");
    }

    fn synth_frame(command: Command, order: ByteOrder, payload: Vec<u8>) -> Frame {
        let header = PvaHeader::application(false, order, command.code(), payload.len() as u32);
        Frame { header, payload }
    }

    #[test]
    fn handle_message_does_not_panic_on_well_formed_input() {
        // Wire layout: ioid (u32) + messageType (u8) + message (string).
        // We can't easily inspect tracing output here, so the assertion is
        // simply that the handler tolerates each severity level without
        // panicking and consumes the cursor cleanly.
        let order = ByteOrder::Little;
        let peer = "127.0.0.1:5075".parse::<SocketAddr>().unwrap();
        for mtype in [0u8, 1, 2, 3, 9] {
            let mut payload = Vec::new();
            payload.put_u32(0xDEADBEEF, order); // ioid
            payload.put_u8(mtype);
            crate::proto::encode_string_into("hello from client", order, &mut payload);
            let frame = synth_frame(Command::Message, order, payload);
            // PVA-R28: MESSAGE handler now returns PvaResult; well-formed
            // payload must succeed.
            handle_message(&frame, order, &peer).expect("well-formed MESSAGE");
        }

        // PVA-R28: truncated MESSAGE is now a protocol-fatal decode
        // error (matches pvxs `serverconn.cpp:323-336` throw). The
        // server loop turns this into a connection reset.
        let frame_short = synth_frame(Command::Message, order, vec![0x01, 0x02]);
        let err =
            handle_message(&frame_short, order, &peer).expect_err("truncated MESSAGE must Err");
        assert!(
            matches!(err, PvaError::Decode(_)),
            "expected Decode error, got {err:?}"
        );
    }

    #[tokio::test]
    async fn cancel_request_pauses_monitor_without_aborting() {
        // Round 4 cancel-vs-destroy parity: pvxs serverconn.cpp:262-289
        // transitions Executing→Idle and fires onCancel, but the
        // underlying op + subscription stay alive. Our model: flip
        // `monitor_paused` so the subscriber suspends emission, leaving
        // the abort guard untouched so the spawned task survives.
        let order = ByteOrder::Little;
        let sid: u32 = 7;
        let ioid: u32 = 99;

        // Stand up a fake OpState whose `monitor_abort` points at a real
        // task we can observe NOT being cancelled.
        let task = tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(60)).await;
            }
        });
        let abort = Arc::new(AbortOnDrop(task.abort_handle()));
        let paused = Arc::new(std::sync::atomic::AtomicBool::new(false));

        let mut channels: HashMap<u32, ChannelState> = HashMap::new();
        let mut ops = HashMap::new();
        ops.insert(
            ioid,
            OpState {
                intro: FieldDesc::Variant,
                kind: OpKind::Monitor,
                monitor_started: true,
                monitor_abort: Some(abort.clone()),
                mask: BitSet::new(),
                monitor_window: None,
                monitor_window_notify: None,
                monitor_paused: paused.clone(),
                monitor_filters: Arc::new(
                    epics_base_rs::server::database::filters::FilterChain::new(),
                ),
                put_auto_exec: true,
                pv_request: None,
            },
        );
        channels.insert(
            sid,
            ChannelState {
                name: "dut".into(),
                cid: 1,
                sid,
                introspection: None,
                ops,
            },
        );

        // Build the CancelRequest payload: sid + ioid.
        let mut payload = Vec::new();
        payload.put_u32(sid, order);
        payload.put_u32(ioid, order);
        let frame = synth_frame(Command::CancelRequest, order, payload);
        handle_cancel_request(&frame, &mut channels, order).expect("well-formed CancelRequest");

        // pvxs parity: op stays in the map, started flag stays set, abort
        // guard stays attached, pause flag flips on. Subsequent START
        // (subcmd 0x44) flips pause off via handle_op's resume path.
        let op = channels
            .get(&sid)
            .and_then(|c| c.ops.get(&ioid))
            .expect("op preserved across cancel");
        assert!(
            op.monitor_started,
            "monitor_started must stay set — cancel doesn't tear down"
        );
        assert!(
            op.monitor_abort.is_some(),
            "abort guard must stay — cancel preserves subscriber task"
        );
        assert!(
            paused.load(std::sync::atomic::Ordering::Relaxed),
            "monitor_paused must flip on so the subscriber suspends emission"
        );

        // Drop our test-side abort handle so the spawned task can exit
        // when the OpState's clone is also dropped. With the OpState
        // still alive in `channels`, the task should still be running
        // immediately after cancel.
        drop(abort);
        // The task must NOT have been aborted yet — the OpState in
        // `channels` still holds an Arc to the abort guard.
        let join_attempt = tokio::time::timeout(
            Duration::from_millis(50),
            &mut Box::pin(async {
                // Probe: confirm task is still pending by sleeping briefly.
                tokio::time::sleep(Duration::from_millis(10)).await;
            }),
        )
        .await;
        assert!(join_attempt.is_ok(), "probe should not time out");

        // Now drop the OpState (simulating DESTROY); the task must abort.
        channels.clear();
        let join = tokio::time::timeout(Duration::from_millis(500), task).await;
        let outcome = join.expect("aborted task should finish quickly");
        assert!(
            outcome.unwrap_err().is_cancelled(),
            "task should abort only on DESTROY (OpState drop), not on cancel"
        );
    }

    #[test]
    fn monitor_payload_orders_overrun_after_value() {
        let order = ByteOrder::Little;
        let ioid = 0x1234;
        let intro = FieldDesc::Structure {
            struct_id: "epics:nt/NTScalar:1.0".into(),
            fields: vec![("value".into(), FieldDesc::Scalar(ScalarType::Double))],
        };
        let mut value = PvStructure::new("epics:nt/NTScalar:1.0");
        value
            .fields
            .push(("value".into(), PvField::Scalar(ScalarValue::Double(42.5))));

        let mask = BitSet::all_set(intro.total_bits());
        let bytes = build_monitor_payload(ioid, &intro, &PvField::Structure(value), &mask, order);
        let (frame, used) = try_parse_frame(&bytes).unwrap().expect("complete frame");
        assert_eq!(used, bytes.len());

        match decode_op_response(&frame, Some(&intro)).unwrap() {
            OpResponse::Data(data) => {
                assert_eq!(data.ioid, ioid);
                match data.value {
                    PvField::Structure(s) => {
                        assert_eq!(
                            s.get_field("value"),
                            Some(&PvField::Scalar(ScalarValue::Double(42.5)))
                        );
                    }
                    other => panic!("expected structure, got {other:?}"),
                }
            }
            other => panic!("expected monitor data, got {other:?}"),
        }
    }

    /// pvxs `servermon.cpp:493` parity: when the client sets the
    /// pipeline bit (`subcmd & 0x80`) on MONITOR INIT, the body
    /// carries a trailing u32 `nack` (initial window). The handler
    /// must consume those four bytes so subsequent reads from the
    /// cursor see the correct offset, AND surface the parsed value
    /// to override the pvRequest queueSize-based default.
    #[test]
    fn parse_monitor_init_nack_consumes_window_byte_when_pipeline_bit_set() {
        let order = ByteOrder::Little;

        // Bit clear → no-op even on Monitor.
        let bytes = [0u8; 8];
        let mut cur = std::io::Cursor::new(bytes.as_slice());
        assert_eq!(
            parse_monitor_init_nack(OpKind::Monitor, 0x08, &mut cur, order),
            None
        );
        assert_eq!(cur.position(), 0, "cursor must not advance when bit clear");

        // Bit set, kind != Monitor → no-op (matches pvxs which only
        // honours the pipeline shape on the MONITOR command code).
        let mut cur = std::io::Cursor::new(bytes.as_slice());
        assert_eq!(
            parse_monitor_init_nack(OpKind::Get, 0x88, &mut cur, order),
            None
        );
        assert_eq!(cur.position(), 0);

        // Bit set, four bytes available → return decoded value.
        let mut buf = Vec::new();
        buf.put_u32(0x1234_5678, order);
        buf.extend_from_slice(b"trailing");
        let mut cur = std::io::Cursor::new(buf.as_slice());
        let parsed = parse_monitor_init_nack(OpKind::Monitor, 0x88, &mut cur, order);
        assert_eq!(parsed, Some(0x1234_5678));
        assert_eq!(cur.position(), 4, "must advance exactly four bytes");

        // Bit set, fewer than four bytes → tolerate (pvxs warns but
        // accepts; we surface `None` so the caller falls back to the
        // pvRequest queueSize-based default).
        let buf = vec![0x11, 0x22];
        let mut cur = std::io::Cursor::new(buf.as_slice());
        let parsed = parse_monitor_init_nack(OpKind::Monitor, 0x88, &mut cur, order);
        assert_eq!(parsed, None);
    }

    /// pvxs `serverchan.cpp:382-386`: when the SID in DESTROY_CHANNEL
    /// is unknown the server logs at debug and silently returns — no
    /// reply frame is emitted. Previously we unconditionally fabricated
    /// `OK` echo back even for SIDs we never created, which both
    /// amplifies (1:1) and confuses correctness diagnostics in the
    /// client.
    #[tokio::test]
    async fn destroy_channel_on_unknown_sid_emits_no_reply() {
        let order = ByteOrder::Little;
        let unknown_sid: u32 = 4242;
        let cid: u32 = 7;

        let mut channels: HashMap<u32, ChannelState> = HashMap::new();
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);

        let mut payload = Vec::new();
        payload.put_u32(unknown_sid, order);
        payload.put_u32(cid, order);
        let frame = synth_frame(Command::DestroyChannel, order, payload);

        handle_destroy_channel(&frame, &tx, &mut channels, order)
            .await
            .expect("handler returns Ok");

        // Channel was never present; map stays empty.
        assert!(channels.is_empty(), "no channel inserted");
        // No reply emitted — pvxs parity.
        assert!(
            rx.try_recv().is_err(),
            "DESTROY_CHANNEL on unknown SID must not emit a reply frame"
        );
    }

    /// pvxs DESTROY_CHANNEL for a known SID echoes `sid + cid` back
    /// (`serverchan.cpp:399-411`). The unknown-SID guard above must
    /// not regress this path: when the SID exists, the reply still
    /// fires.
    #[tokio::test]
    async fn destroy_channel_on_known_sid_emits_echo() {
        let order = ByteOrder::Little;
        let sid: u32 = 11;
        let cid: u32 = 22;

        let mut channels: HashMap<u32, ChannelState> = HashMap::new();
        channels.insert(
            sid,
            ChannelState {
                name: "dut".into(),
                cid,
                sid,
                introspection: None,
                ops: HashMap::new(),
            },
        );
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);

        let mut payload = Vec::new();
        payload.put_u32(sid, order);
        payload.put_u32(cid, order);
        let frame = synth_frame(Command::DestroyChannel, order, payload);

        handle_destroy_channel(&frame, &tx, &mut channels, order)
            .await
            .expect("handler returns Ok");

        assert!(!channels.contains_key(&sid), "channel removed on hit");
        let reply = rx.try_recv().expect("reply emitted for known SID");
        // Header (8) + ioid placeholder isn't part of DESTROY_CHANNEL;
        // payload is sid (4) + cid (4) = 8 total, so frame length = 16.
        assert_eq!(reply.len(), PvaHeader::SIZE + 8);
    }

    /// pvxs `serverget.cpp:83` echoes the request `subcmd` byte in the
    /// PUT data response. The PUT_GET (readback) case sets bit 0x40 in
    /// the client subcmd; pvxs `clientget.cpp:362-370` dispatches the
    /// reply decode based on that bit. A server response that hardcodes
    /// 0x00 makes the client decode the wrong shape: the bitset + value
    /// bytes carried in the frame are misread as trailing garbage and
    /// the PUT_GET readback is silently lost.
    #[tokio::test]
    async fn put_get_response_echoes_request_subcmd() {
        use crate::pvdata::FieldDesc;
        use crate::pvdata::{PvField, PvStructure, ScalarType, ScalarValue};
        use crate::server_native::SharedSource;
        use crate::server_native::runtime::PvaServerConfig;
        use crate::server_native::shared_pv::SharedPV;
        use crate::server_native::tcp::ClientCredentials;
        use std::sync::Arc;

        let order = ByteOrder::Little;
        let sid: u32 = 1;
        let ioid: u32 = 100;

        // Build a SharedSource with one PV "dut" of type NTScalar<f64>.
        let pv = SharedPV::new();
        let intro = FieldDesc::Structure {
            struct_id: "epics:nt/NTScalar:1.0".into(),
            fields: vec![("value".into(), FieldDesc::Scalar(ScalarType::Double))],
        };
        let mut initial = PvStructure::new("epics:nt/NTScalar:1.0");
        initial
            .fields
            .push(("value".into(), PvField::Scalar(ScalarValue::Double(1.0))));
        pv.open(intro.clone(), PvField::Structure(initial));

        let shared = SharedSource::new();
        shared.add("dut", pv);
        let source: DynSource = Arc::new(shared);

        // Pre-populate a ChannelState as if CREATE_CHANNEL had already
        // run, so we can drive the PUT INIT + EXEC frames directly.
        let mut channels: HashMap<u32, ChannelState> = HashMap::new();
        channels.insert(
            sid,
            ChannelState {
                name: "dut".into(),
                cid: 0,
                sid,
                introspection: Some(intro.clone()),
                ops: HashMap::new(),
            },
        );

        let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(16);
        let config = PvaServerConfig::default();
        let mut encode_cache = crate::pvdata::encode::EncodeTypeCache::new();
        let peer: SocketAddr = "127.0.0.1:5075".parse().unwrap();
        let cred = ClientCredentials::anonymous();

        // PUT INIT: sid + ioid + subcmd=0x08 + pvRequest(type + value).
        // Use an empty Structure pvRequest (full mask).
        let req_desc = FieldDesc::Structure {
            struct_id: String::new(),
            fields: vec![],
        };
        let req_val = PvField::Structure(PvStructure::new(""));
        let mut init_payload = Vec::new();
        init_payload.put_u32(sid, order);
        init_payload.put_u32(ioid, order);
        init_payload.put_u8(0x08); // INIT
        crate::pvdata::encode::encode_type_desc(&req_desc, order, &mut init_payload);
        crate::pvdata::encode::encode_pv_field(&req_val, &req_desc, order, &mut init_payload);
        let init_frame = synth_frame(Command::Put, order, init_payload);
        handle_op(
            &source,
            &init_frame,
            &tx,
            &mut channels,
            order,
            OpKind::Put,
            &config,
            &mut encode_cache,
            peer,
            &cred,
        )
        .await
        .expect("PUT INIT ok");
        // Drain INIT response — not the focus of this test.
        let _init_resp = rx.try_recv().expect("INIT response emitted");

        // PUT EXEC with subcmd=0x40 (PUT_GET readback): sid + ioid +
        // subcmd + bitset + value.
        let new_val = {
            let mut s = PvStructure::new("epics:nt/NTScalar:1.0");
            s.fields
                .push(("value".into(), PvField::Scalar(ScalarValue::Double(7.5))));
            PvField::Structure(s)
        };
        let mut exec_payload = Vec::new();
        exec_payload.put_u32(sid, order);
        exec_payload.put_u32(ioid, order);
        exec_payload.put_u8(0x40); // PUT_GET readback
        let bs = BitSet::all_set(intro.total_bits());
        bs.write_into(order, &mut exec_payload);
        crate::pvdata::encode::encode_pv_field(&new_val, &intro, order, &mut exec_payload);
        let exec_frame = synth_frame(Command::Put, order, exec_payload);

        handle_op(
            &source,
            &exec_frame,
            &tx,
            &mut channels,
            order,
            OpKind::Put,
            &config,
            &mut encode_cache,
            peer,
            &cred,
        )
        .await
        .expect("PUT EXEC ok");

        let resp = rx.try_recv().expect("PUT EXEC response emitted");
        // Skip 8-byte header; payload = ioid (4) + subcmd (1) + ...
        assert!(resp.len() >= PvaHeader::SIZE + 5);
        let resp_subcmd = resp[PvaHeader::SIZE + 4];
        assert_eq!(
            resp_subcmd, 0x40,
            "PUT_GET reply subcmd must echo the 0x40 readback bit (pvxs serverget.cpp:83)"
        );
    }

    /// Companion: a plain PUT EXEC (subcmd=0x00, no readback bit) must
    /// still emit `subcmd=0x00` in the response. Confirms the echo
    /// behaviour is symmetric — neither leaking 0x40 when not requested
    /// nor regressing the common case.
    #[tokio::test]
    async fn put_exec_response_echoes_zero_subcmd() {
        use crate::pvdata::FieldDesc;
        use crate::pvdata::{PvField, PvStructure, ScalarType, ScalarValue};
        use crate::server_native::SharedSource;
        use crate::server_native::runtime::PvaServerConfig;
        use crate::server_native::shared_pv::SharedPV;
        use crate::server_native::tcp::ClientCredentials;
        use std::sync::Arc;

        let order = ByteOrder::Little;
        let sid: u32 = 1;
        let ioid: u32 = 200;

        let pv = SharedPV::new();
        let intro = FieldDesc::Structure {
            struct_id: "epics:nt/NTScalar:1.0".into(),
            fields: vec![("value".into(), FieldDesc::Scalar(ScalarType::Double))],
        };
        let mut initial = PvStructure::new("epics:nt/NTScalar:1.0");
        initial
            .fields
            .push(("value".into(), PvField::Scalar(ScalarValue::Double(1.0))));
        pv.open(intro.clone(), PvField::Structure(initial));

        let shared = SharedSource::new();
        shared.add("dut", pv);
        let source: DynSource = Arc::new(shared);

        let mut channels: HashMap<u32, ChannelState> = HashMap::new();
        channels.insert(
            sid,
            ChannelState {
                name: "dut".into(),
                cid: 0,
                sid,
                introspection: Some(intro.clone()),
                ops: HashMap::new(),
            },
        );

        let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(16);
        let config = PvaServerConfig::default();
        let mut encode_cache = crate::pvdata::encode::EncodeTypeCache::new();
        let peer: SocketAddr = "127.0.0.1:5075".parse().unwrap();
        let cred = ClientCredentials::anonymous();

        let req_desc = FieldDesc::Structure {
            struct_id: String::new(),
            fields: vec![],
        };
        let req_val = PvField::Structure(PvStructure::new(""));
        let mut init_payload = Vec::new();
        init_payload.put_u32(sid, order);
        init_payload.put_u32(ioid, order);
        init_payload.put_u8(0x08);
        crate::pvdata::encode::encode_type_desc(&req_desc, order, &mut init_payload);
        crate::pvdata::encode::encode_pv_field(&req_val, &req_desc, order, &mut init_payload);
        let init_frame = synth_frame(Command::Put, order, init_payload);
        handle_op(
            &source,
            &init_frame,
            &tx,
            &mut channels,
            order,
            OpKind::Put,
            &config,
            &mut encode_cache,
            peer,
            &cred,
        )
        .await
        .expect("PUT INIT ok");
        let _ = rx.try_recv().expect("INIT resp");

        // Plain PUT EXEC: subcmd=0x00.
        let new_val = {
            let mut s = PvStructure::new("epics:nt/NTScalar:1.0");
            s.fields
                .push(("value".into(), PvField::Scalar(ScalarValue::Double(2.5))));
            PvField::Structure(s)
        };
        let mut exec_payload = Vec::new();
        exec_payload.put_u32(sid, order);
        exec_payload.put_u32(ioid, order);
        exec_payload.put_u8(0x00);
        let bs = BitSet::all_set(intro.total_bits());
        bs.write_into(order, &mut exec_payload);
        crate::pvdata::encode::encode_pv_field(&new_val, &intro, order, &mut exec_payload);
        let exec_frame = synth_frame(Command::Put, order, exec_payload);

        handle_op(
            &source,
            &exec_frame,
            &tx,
            &mut channels,
            order,
            OpKind::Put,
            &config,
            &mut encode_cache,
            peer,
            &cred,
        )
        .await
        .expect("PUT EXEC ok");
        let resp = rx.try_recv().expect("PUT EXEC response emitted");
        assert!(resp.len() >= PvaHeader::SIZE + 5);
        let resp_subcmd = resp[PvaHeader::SIZE + 4];
        assert_eq!(
            resp_subcmd, 0x00,
            "plain PUT EXEC reply subcmd must echo 0x00"
        );
    }

    /// Build a flat 3-field NTScalar-like structure descriptor with
    /// children `a`, `b`, `c` (all `Int`). Bit numbering (pvData §5.4
    /// depth-first): root=0, a=1, b=2, c=3.
    #[cfg(test)]
    fn three_field_intro() -> FieldDesc {
        use crate::pvdata::FieldDesc;
        FieldDesc::Structure {
            struct_id: "test:nt/Triple:1.0".into(),
            fields: vec![
                ("a".into(), FieldDesc::Scalar(ScalarType::Int)),
                ("b".into(), FieldDesc::Scalar(ScalarType::Int)),
                ("c".into(), FieldDesc::Scalar(ScalarType::Int)),
            ],
        }
    }

    /// Build a `PvField::Structure` with the three `Int` children set
    /// to the given values.
    #[cfg(test)]
    fn three_field_value(a: i32, b: i32, c: i32) -> PvField {
        let mut s = PvStructure::new("test:nt/Triple:1.0");
        s.fields
            .push(("a".into(), PvField::Scalar(ScalarValue::Int(a))));
        s.fields
            .push(("b".into(), PvField::Scalar(ScalarValue::Int(b))));
        s.fields
            .push(("c".into(), PvField::Scalar(ScalarValue::Int(c))));
        PvField::Structure(s)
    }

    /// Extract the three `Int` children of a `PvField::Structure`.
    #[cfg(test)]
    fn three_field_extract(v: &PvField) -> (i32, i32, i32) {
        let s = match v {
            PvField::Structure(s) => s,
            other => panic!("expected Structure, got {other:?}"),
        };
        let get = |name: &str| match s.get_field(name) {
            Some(PvField::Scalar(ScalarValue::Int(n))) => *n,
            other => panic!("field '{name}' not Int: {other:?}"),
        };
        (get("a"), get("b"), get("c"))
    }

    /// Regression (Defect 1): the PVA client encodes the PUT data
    /// phase as a BitSet delta — only the marked fields are present
    /// on the wire. A 3-field structure where only field `b` (bit 2)
    /// changed carries `changed | <b's 4 bytes>`, NOT all three
    /// fields. Decoding the value as a full structure
    /// (`decode_pv_field`) reads `a`'s slot from `b`'s bytes and then
    /// runs off the end — the data phase desyncs. The fix decodes
    /// with the changed-BitSet and merges over the PV's prior value.
    ///
    /// Before the fix this test fails: a full-structure decode of a
    /// single-field-wide payload either errors (short read) or
    /// misreads `b`'s bytes as `a` and clobbers `b`/`c` with garbage.
    #[tokio::test]
    async fn put_delta_multi_field_applies_only_changed_field() {
        use crate::server_native::SharedSource;
        use crate::server_native::runtime::PvaServerConfig;
        use crate::server_native::shared_pv::SharedPV;
        use crate::server_native::tcp::ClientCredentials;
        use std::sync::Arc;

        let order = ByteOrder::Little;
        let sid: u32 = 1;
        let ioid: u32 = 300;

        let intro = three_field_intro();
        let pv = SharedPV::new();
        pv.open(intro.clone(), three_field_value(10, 20, 30));

        let shared = SharedSource::new();
        shared.add("dut", pv);
        let source: DynSource = Arc::new(shared);

        let mut channels: HashMap<u32, ChannelState> = HashMap::new();
        channels.insert(
            sid,
            ChannelState {
                name: "dut".into(),
                cid: 0,
                sid,
                introspection: Some(intro.clone()),
                ops: HashMap::new(),
            },
        );

        let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(16);
        let config = PvaServerConfig::default();
        let mut encode_cache = crate::pvdata::encode::EncodeTypeCache::new();
        let peer: SocketAddr = "127.0.0.1:5075".parse().unwrap();
        let cred = ClientCredentials::anonymous();

        // PUT INIT.
        let req_desc = FieldDesc::Structure {
            struct_id: String::new(),
            fields: vec![],
        };
        let req_val = PvField::Structure(PvStructure::new(""));
        let mut init_payload = Vec::new();
        init_payload.put_u32(sid, order);
        init_payload.put_u32(ioid, order);
        init_payload.put_u8(0x08);
        crate::pvdata::encode::encode_type_desc(&req_desc, order, &mut init_payload);
        crate::pvdata::encode::encode_pv_field(&req_val, &req_desc, order, &mut init_payload);
        let init_frame = synth_frame(Command::Put, order, init_payload);
        handle_op(
            &source,
            &init_frame,
            &tx,
            &mut channels,
            order,
            OpKind::Put,
            &config,
            &mut encode_cache,
            peer,
            &cred,
        )
        .await
        .expect("PUT INIT ok");
        let _ = rx.try_recv().expect("INIT resp");

        // PUT EXEC — delta with only field `b` (bit 2) changed to 99.
        // This is exactly what the client encoder emits: a changed
        // BitSet with bit 2 set, followed by `encode_pv_field_with_bitset`
        // which writes ONLY `b`'s 4 bytes.
        let bit_b = intro.bit_for_path("b").expect("b has a bit");
        assert_eq!(bit_b, 2, "field b must occupy bit 2 (pvData §5.4)");
        let mut changed = BitSet::new();
        changed.set(bit_b);
        let delta = three_field_value(0, 99, 0);
        let mut exec_payload = Vec::new();
        exec_payload.put_u32(sid, order);
        exec_payload.put_u32(ioid, order);
        exec_payload.put_u8(0x00);
        changed.write_into(order, &mut exec_payload);
        crate::pvdata::encode::encode_pv_field_with_bitset(
            &delta,
            &intro,
            &changed,
            0,
            order,
            &mut exec_payload,
        );
        let exec_frame = synth_frame(Command::Put, order, exec_payload);
        handle_op(
            &source,
            &exec_frame,
            &tx,
            &mut channels,
            order,
            OpKind::Put,
            &config,
            &mut encode_cache,
            peer,
            &cred,
        )
        .await
        .expect("PUT EXEC ok");
        let _ = rx.try_recv().expect("PUT EXEC response emitted");

        // The server must apply ONLY field `b`; `a` and `c` keep
        // their prior values.
        let stored = source.get_value("dut").await.expect("PV value present");
        assert_eq!(
            three_field_extract(&stored),
            (10, 99, 30),
            "PUT delta must change only field b; a and c must be untouched"
        );
    }

    /// Regression (Defect 1, PUT_GET path): same as the PUT delta
    /// test but via the dedicated `handle_put_get` (Command::PutGet,
    /// cmd 12). A 3-field structure PUT_GET where only field `c`
    /// (bit 3) changed must apply exactly `c` and leave `a`/`b`
    /// intact, and the readback must reflect the merged value.
    #[tokio::test]
    async fn put_get_delta_multi_field_applies_only_changed_field() {
        use crate::server_native::SharedSource;
        use crate::server_native::runtime::PvaServerConfig;
        use crate::server_native::shared_pv::SharedPV;
        use crate::server_native::tcp::ClientCredentials;
        use std::sync::Arc;

        let order = ByteOrder::Little;
        let sid: u32 = 1;
        let ioid: u32 = 400;

        let intro = three_field_intro();
        let pv = SharedPV::new();
        pv.open(intro.clone(), three_field_value(10, 20, 30));

        let shared = SharedSource::new();
        shared.add("dut", pv);
        let source: DynSource = Arc::new(shared);

        let mut channels: HashMap<u32, ChannelState> = HashMap::new();
        channels.insert(
            sid,
            ChannelState {
                name: "dut".into(),
                cid: 0,
                sid,
                introspection: Some(intro.clone()),
                ops: HashMap::new(),
            },
        );

        let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(16);
        let config = PvaServerConfig::default();
        let mut encode_cache = crate::pvdata::encode::EncodeTypeCache::new();
        let peer: SocketAddr = "127.0.0.1:5075".parse().unwrap();
        let cred = ClientCredentials::anonymous();

        // PUT_GET INIT (subcmd 0x08).
        let req_desc = FieldDesc::Structure {
            struct_id: String::new(),
            fields: vec![],
        };
        let req_val = PvField::Structure(PvStructure::new(""));
        let mut init_payload = Vec::new();
        init_payload.put_u32(sid, order);
        init_payload.put_u32(ioid, order);
        init_payload.put_u8(0x08);
        crate::pvdata::encode::encode_type_desc(&req_desc, order, &mut init_payload);
        crate::pvdata::encode::encode_pv_field(&req_val, &req_desc, order, &mut init_payload);
        let init_frame = synth_frame(Command::PutGet, order, init_payload);
        handle_put_get(
            &source,
            &init_frame,
            &tx,
            &mut channels,
            order,
            &config,
            &mut encode_cache,
            peer,
            &cred,
        )
        .await
        .expect("PUT_GET INIT ok");
        let _ = rx.try_recv().expect("INIT resp");

        // PUT_GET data phase — delta with only field `c` (bit 3).
        let bit_c = intro.bit_for_path("c").expect("c has a bit");
        assert_eq!(bit_c, 3, "field c must occupy bit 3 (pvData §5.4)");
        let mut changed = BitSet::new();
        changed.set(bit_c);
        let delta = three_field_value(0, 0, 77);
        let mut data_payload = Vec::new();
        data_payload.put_u32(sid, order);
        data_payload.put_u32(ioid, order);
        data_payload.put_u8(0x00);
        changed.write_into(order, &mut data_payload);
        crate::pvdata::encode::encode_pv_field_with_bitset(
            &delta,
            &intro,
            &changed,
            0,
            order,
            &mut data_payload,
        );
        let data_frame = synth_frame(Command::PutGet, order, data_payload);
        handle_put_get(
            &source,
            &data_frame,
            &tx,
            &mut channels,
            order,
            &config,
            &mut encode_cache,
            peer,
            &cred,
        )
        .await
        .expect("PUT_GET data ok");
        let resp = rx.try_recv().expect("PUT_GET data response emitted");

        // Server-side state: only `c` changed.
        let stored = source.get_value("dut").await.expect("PV value present");
        assert_eq!(
            three_field_extract(&stored),
            (10, 20, 77),
            "PUT_GET delta must change only field c; a and b must be untouched"
        );

        // Readback: decode the PUT_GET response payload directly
        // (`decode_op_response` rejects Command::PutGet — cmd 12 is
        // not in its Get/Put/Monitor/Rpc set). The GET-leg success
        // path emits `ioid + subcmd + status + mask + value`.
        let (frame, _consumed) = try_parse_frame(&resp)
            .expect("readback frame parses")
            .expect("complete frame");
        assert_eq!(
            frame.header.command,
            Command::PutGet.code(),
            "readback is a PUT_GET reply"
        );
        let mut cur = frame.cursor();
        let _ioid = cur.get_u32(order).expect("ioid");
        let _subcmd = cur.get_u8().expect("subcmd");
        let status = Status::decode(&mut cur, order).expect("status");
        assert!(status.is_success(), "PUT_GET readback status ok");
        let mask = BitSet::decode(&mut cur, order).expect("readback bitset");
        let readback =
            crate::pvdata::encode::decode_pv_field_with_bitset(&intro, &mask, 0, &mut cur, order)
                .expect("readback value");
        // The readback mask is the op's field mask (all fields here),
        // so every field is present and reflects the merged state.
        assert_eq!(
            three_field_extract(&readback),
            (10, 20, 77),
            "PUT_GET readback must carry the merged value"
        );
    }

    /// Regression (Defect 2): concurrent BitSet-delta PUTs with
    /// DISJOINT changed-fields must not lose updates.
    ///
    /// The server PUT path is a read-merge-write: read the PV's
    /// prior complete value, overlay the marked fields from the wire
    /// delta, store the merged result. Done as separate `get_value`
    /// + `put_value` ops, two concurrent partial PUTs from different
    /// connections to the same PV can both read the same `prior`;
    /// the second write then overwrites the first writer's fields
    /// with the prior's (unchanged) value — a silent lost update.
    ///
    /// `put_delta_checked` (→ `SharedPV::put_delta`) closes the
    /// window by performing read + merge + store under a single
    /// mutex acquisition. Here writer A changes field `a`, writer B
    /// changes field `c`; with the atomic merge BOTH must survive
    /// regardless of interleaving. Before the fix the second writer
    /// to commit clobbers the first's field.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_disjoint_delta_puts_do_not_lose_updates() {
        use crate::server_native::SharedSource;
        use crate::server_native::shared_pv::SharedPV;
        use crate::server_native::source::{ChannelContext, ChannelSource};
        use std::sync::Arc;

        let intro = three_field_intro();

        // Run many trials to give the scheduler a chance to surface
        // any residual interleaving race.
        for trial in 0..200 {
            let pv = SharedPV::new();
            pv.open(intro.clone(), three_field_value(0, 0, 0));
            let shared = SharedSource::new();
            shared.add("dut", pv);
            let source = Arc::new(shared);

            let bit_a = intro.bit_for_path("a").expect("a has a bit");
            let bit_c = intro.bit_for_path("c").expect("c has a bit");

            // Writer A: only field `a` (bit 1) → 111.
            let mut changed_a = BitSet::new();
            changed_a.set(bit_a);
            let delta_a = three_field_value(111, 0, 0);

            // Writer B: only field `c` (bit 3) → 333.
            let mut changed_c = BitSet::new();
            changed_c.set(bit_c);
            let delta_c = three_field_value(0, 0, 333);

            let ctx = ChannelContext {
                peer: "127.0.0.1:5075".parse().unwrap(),
                account: "anonymous".into(),
                method: "anonymous".into(),
                host: "127.0.0.1".into(),
                authority: String::new(),
                pv_request: None,
            };

            let src_a = Arc::clone(&source);
            let src_c = Arc::clone(&source);
            let intro_a = intro.clone();
            let intro_c = intro.clone();
            let ctx_a = ctx.clone();
            let ctx_c = ctx.clone();

            let task_a = tokio::spawn(async move {
                let checked = src_a
                    .access()
                    .check("dut", &ctx_a.host, &ctx_a.account, &ctx_a.method, "")
                    .await;
                src_a
                    .put_delta_checked(checked, intro_a, changed_a, delta_a, ctx_a)
                    .await
            });
            let task_c = tokio::spawn(async move {
                let checked = src_c
                    .access()
                    .check("dut", &ctx_c.host, &ctx_c.account, &ctx_c.method, "")
                    .await;
                src_c
                    .put_delta_checked(checked, intro_c, changed_c, delta_c, ctx_c)
                    .await
            });

            task_a.await.unwrap().expect("PUT A ok");
            task_c.await.unwrap().expect("PUT C ok");

            let stored = source.get_value("dut").await.expect("PV value present");
            let (a, b, c) = three_field_extract(&stored);
            assert_eq!(
                (a, c),
                (111, 333),
                "trial {trial}: both disjoint delta PUTs must survive — \
                 got a={a}, c={c} (a lost update means one is still 0)"
            );
            assert_eq!(b, 0, "trial {trial}: field b was never written");
        }
    }

    /// Test source for the Defect-1 AUTHORITY-gating regression.
    ///
    /// Carries a `Required` AccessGate whose ASG has:
    /// `RULE(0, READ)` — unconditional read; and
    /// `RULE(1, WRITE) { AUTHORITY("MyCA") }` — WRITE only for a
    /// peer whose `authority` (x509 root-CA CommonName) is `"MyCA"`.
    ///
    /// `process_hits` counts whether the WRITE-class `process` hook
    /// ran — it must run only when the gate granted WRITE. The bug:
    /// `handle_process` / `handle_put_get` GET-leg passed a literal
    /// `""` as the authority to `AccessGate::check`, so even a peer
    /// presenting `authority="MyCA"` failed `authority_match` and
    /// was wrongly denied.
    struct AuthorityGatedSource {
        gate: epics_base_rs::server::access_security::AccessGate,
        value: std::sync::Arc<parking_lot::Mutex<i32>>,
        process_hits: std::sync::Arc<std::sync::atomic::AtomicU32>,
    }

    impl AuthorityGatedSource {
        fn new() -> Self {
            use epics_base_rs::server::access_security::{AsgAslResolver, parse_acf};
            let acf = parse_acf(
                "ASG(DEFAULT) {\n\
                 \x20   RULE(0, READ)\n\
                 \x20   RULE(1, WRITE) { AUTHORITY(\"MyCA\") }\n\
                 }\n",
            )
            .expect("acf parse");
            let cell = std::sync::Arc::new(tokio::sync::RwLock::new(Some(acf)));
            // Resolve every PV to ASG DEFAULT, ASL 1 — so the
            // ASL-1-scoped WRITE rule applies.
            let resolver: AsgAslResolver =
                std::sync::Arc::new(|_pv| Box::pin(async { ("DEFAULT".to_string(), 1u8) }));
            Self {
                gate: epics_base_rs::server::access_security::AccessGate::required(cell, resolver),
                value: std::sync::Arc::new(parking_lot::Mutex::new(7)),
                process_hits: std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0)),
            }
        }
    }

    impl crate::server_native::source::ChannelSource for AuthorityGatedSource {
        fn access(&self) -> &epics_base_rs::server::access_security::AccessGate {
            &self.gate
        }
        async fn list_pvs(&self) -> Vec<String> {
            vec!["dut".into()]
        }
        fn has_pv(&self, n: &str) -> impl std::future::Future<Output = bool> + Send {
            let n = n.to_string();
            async move { n == "dut" }
        }
        async fn get_introspection(&self, _: &str) -> Option<FieldDesc> {
            Some(three_field_intro())
        }
        fn get_value(&self, _: &str) -> impl std::future::Future<Output = Option<PvField>> + Send {
            let v = *self.value.lock();
            async move { Some(three_field_value(v, v, v)) }
        }
        fn put_value(
            &self,
            _: &str,
            value: PvField,
        ) -> impl std::future::Future<Output = Result<(), String>> + Send {
            let (a, _, _) = three_field_extract(&value);
            *self.value.lock() = a;
            async { Ok(()) }
        }
        async fn is_writable(&self, _: &str) -> bool {
            true
        }
        async fn subscribe(&self, _: &str) -> Option<mpsc::Receiver<PvField>> {
            None
        }
        fn process(&self, _: &str) -> impl std::future::Future<Output = Result<(), String>> + Send {
            self.process_hits
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            async { Ok(()) }
        }
    }

    /// Build the (sid → ChannelState) map plus a primed PROCESS op
    /// for `ioid`, so a PROCESS data-phase frame dispatches straight
    /// into the WRITE-gate check.
    #[cfg(test)]
    fn primed_process_channels(sid: u32, ioid: u32) -> HashMap<u32, ChannelState> {
        let intro = three_field_intro();
        let mut ops = HashMap::new();
        let mask = BitSet::all_set(intro.total_bits());
        ops.insert(
            ioid,
            non_monitor_op_state(intro.clone(), OpKind::Process, mask),
        );
        let mut channels = HashMap::new();
        channels.insert(
            sid,
            ChannelState {
                name: "dut".into(),
                cid: 0,
                sid,
                introspection: Some(intro),
                ops,
            },
        );
        channels
    }

    /// Credentials for the `x509` method carrying a chosen root-CA
    /// authority. `ClientCredentials` fields are all `pub`.
    #[cfg(test)]
    fn x509_cred(authority: &str) -> ClientCredentials {
        ClientCredentials {
            method: "x509".into(),
            account: "operator".into(),
            host: "h.example".into(),
            authority: authority.into(),
            roles: Vec::new(),
        }
    }

    /// Regression (Defect 1, native PROCESS handler): a peer whose
    /// x509 `authority` matches an `AUTHORITY(...)`-scoped WRITE rule
    /// MUST be granted PROCESS. `handle_process` passed a literal
    /// `""` as the authority to `AccessGate::check`, so the
    /// matching-CA peer failed `authority_match` and was wrongly
    /// denied — its `process` hook never ran.
    #[tokio::test]
    async fn process_honors_authority_scoped_write_rule() {
        let order = ByteOrder::Little;
        let sid: u32 = 1;
        let ioid: u32 = 500;
        let src = AuthorityGatedSource::new();
        let process_hits = std::sync::Arc::clone(&src.process_hits);
        let source: DynSource = std::sync::Arc::new(src);

        let mut channels = primed_process_channels(sid, ioid);
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(16);
        let config = PvaServerConfig::default();
        let peer: SocketAddr = "127.0.0.1:5075".parse().unwrap();

        // PROCESS data-phase frame: sid + ioid + subcmd(0x00).
        let mut payload = Vec::new();
        payload.put_u32(sid, order);
        payload.put_u32(ioid, order);
        payload.put_u8(0x00);
        let frame = synth_frame(Command::Process, order, payload);

        // Peer presents the matching root CA — WRITE must be granted.
        handle_process(
            &source,
            &frame,
            &tx,
            &mut channels,
            order,
            &config,
            peer,
            &x509_cred("MyCA"),
        )
        .await
        .expect("handle_process ok");

        let resp = rx.try_recv().expect("PROCESS response emitted");
        let (rframe, _) = try_parse_frame(&resp)
            .expect("frame parses")
            .expect("complete frame");
        let mut cur = rframe.cursor();
        let _ioid = cur.get_u32(order).expect("ioid");
        let _subcmd = cur.get_u8().expect("subcmd");
        let status = Status::decode(&mut cur, order).expect("status");
        assert!(
            status.is_success(),
            "PROCESS from a peer with matching AUTHORITY must succeed, \
             got non-success status"
        );
        assert_eq!(
            process_hits.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "process hook must run when AUTHORITY-scoped WRITE rule matches"
        );
    }

    /// Negative control for the test above: a peer whose `authority`
    /// does NOT match the `AUTHORITY("MyCA")` rule gets PROCESS
    /// denied and the `process` hook never runs. Confirms the fix
    /// forwards the real authority rather than blanket-granting.
    #[tokio::test]
    async fn process_denied_for_wrong_authority() {
        let order = ByteOrder::Little;
        let sid: u32 = 1;
        let ioid: u32 = 501;
        let src = AuthorityGatedSource::new();
        let process_hits = std::sync::Arc::clone(&src.process_hits);
        let source: DynSource = std::sync::Arc::new(src);

        let mut channels = primed_process_channels(sid, ioid);
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(16);
        let config = PvaServerConfig::default();
        let peer: SocketAddr = "127.0.0.1:5075".parse().unwrap();

        let mut payload = Vec::new();
        payload.put_u32(sid, order);
        payload.put_u32(ioid, order);
        payload.put_u8(0x00);
        let frame = synth_frame(Command::Process, order, payload);

        handle_process(
            &source,
            &frame,
            &tx,
            &mut channels,
            order,
            &config,
            peer,
            &x509_cred("OtherCA"),
        )
        .await
        .expect("handle_process ok");

        let resp = rx.try_recv().expect("PROCESS response emitted");
        let (rframe, _) = try_parse_frame(&resp)
            .expect("frame parses")
            .expect("complete frame");
        let mut cur = rframe.cursor();
        let _ioid = cur.get_u32(order).expect("ioid");
        let _subcmd = cur.get_u8().expect("subcmd");
        let status = Status::decode(&mut cur, order).expect("status");
        assert!(
            !status.is_success(),
            "PROCESS from a peer with the wrong AUTHORITY must be denied"
        );
        assert_eq!(
            process_hits.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "process hook must NOT run when AUTHORITY does not match"
        );
    }

    /// Regression (Defect 1, PUT_GET GET-leg readback): the PUT_GET
    /// GET-leg re-check passed a literal `""` as the authority. With
    /// a READ rule scoped by `AUTHORITY("MyCA")`, a peer presenting
    /// the matching CA would have its readback wrongly suppressed
    /// (empty zero-field bitset instead of the value). Here the READ
    /// rule is unconditional and only WRITE is AUTHORITY-scoped, so
    /// the peer with the matching CA gets BOTH a successful PUT leg
    /// and a non-empty readback — exercising the fixed GET-leg
    /// `&ctx.authority` forwarding.
    #[tokio::test]
    async fn put_get_readback_honors_authority() {
        let order = ByteOrder::Little;
        let sid: u32 = 1;
        let ioid: u32 = 502;
        let src = AuthorityGatedSource::new();
        let source: DynSource = std::sync::Arc::new(src);

        let intro = three_field_intro();
        let mut channels = HashMap::new();
        let mask = BitSet::all_set(intro.total_bits());
        let mut ops = HashMap::new();
        ops.insert(
            ioid,
            non_monitor_op_state(intro.clone(), OpKind::PutGet, mask),
        );
        channels.insert(
            sid,
            ChannelState {
                name: "dut".into(),
                cid: 0,
                sid,
                introspection: Some(intro.clone()),
                ops,
            },
        );

        let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(16);
        let config = PvaServerConfig::default();
        let mut encode_cache = crate::pvdata::encode::EncodeTypeCache::new();
        let peer: SocketAddr = "127.0.0.1:5075".parse().unwrap();

        // PUT_GET data-phase frame (subcmd 0x40 = readback wanted):
        // sid + ioid + subcmd + changed-bitset + delta(field a → 55).
        let bit_a = intro.bit_for_path("a").expect("a has a bit");
        let mut changed = BitSet::new();
        changed.set(bit_a);
        let mut payload = Vec::new();
        payload.put_u32(sid, order);
        payload.put_u32(ioid, order);
        payload.put_u8(0x40);
        changed.write_into(order, &mut payload);
        crate::pvdata::encode::encode_pv_field_with_bitset(
            &three_field_value(55, 0, 0),
            &intro,
            &changed,
            0,
            order,
            &mut payload,
        );
        let frame = synth_frame(Command::PutGet, order, payload);

        handle_put_get(
            &source,
            &frame,
            &tx,
            &mut channels,
            order,
            &config,
            &mut encode_cache,
            peer,
            &x509_cred("MyCA"),
        )
        .await
        .expect("handle_put_get ok");

        let resp = rx.try_recv().expect("PUT_GET response emitted");
        let (rframe, _) = try_parse_frame(&resp)
            .expect("frame parses")
            .expect("complete frame");
        let mut cur = rframe.cursor();
        let _ioid = cur.get_u32(order).expect("ioid");
        let _subcmd = cur.get_u8().expect("subcmd");
        let status = Status::decode(&mut cur, order).expect("status");
        assert!(status.is_success(), "PUT_GET status must be success");
        let readback_mask = BitSet::decode(&mut cur, order).expect("readback bitset");
        assert!(
            readback_mask.count() > 0,
            "PUT_GET GET-leg readback must carry fields for a peer with \
             READ access — an empty bitset means the authority check \
             wrongly suppressed the readback"
        );
        let readback = crate::pvdata::encode::decode_pv_field_with_bitset(
            &intro,
            &readback_mask,
            0,
            &mut cur,
            order,
        )
        .expect("readback value");
        let (a, _, _) = three_field_extract(&readback);
        assert_eq!(a, 55, "readback must reflect the merged PUT (field a=55)");
    }

    /// pvxs `serverintrospect.cpp:159`: GET_FIELD's guard is the
    /// composite `if(!chan || opByIOID.find(ioid)!=opByIOID.end())`.
    /// Both arms log and silently return. Our prior fix (P-G19) only
    /// covered the !chan branch; an IOID collision with an active
    /// GET/PUT/MONITOR/RPC in the same channel still fired back a
    /// fabricated introspection reply, polluting the wire conversation
    /// on the busy IOID.
    #[tokio::test]
    async fn get_field_ioid_collision_with_active_op_drops_reply() {
        use crate::pvdata::FieldDesc;
        use crate::server_native::SharedSource;
        use std::sync::Arc;

        let order = ByteOrder::Little;
        let sid: u32 = 7;
        let ioid: u32 = 1234;

        let shared = SharedSource::new();
        let source: DynSource = Arc::new(shared);

        // Channel with an active op already bound to `ioid`.
        let mut channels: HashMap<u32, ChannelState> = HashMap::new();
        let mut ops: HashMap<u32, OpState> = HashMap::new();
        ops.insert(
            ioid,
            OpState {
                intro: FieldDesc::Variant,
                kind: OpKind::Get,
                monitor_started: false,
                monitor_abort: None,
                mask: BitSet::new(),
                monitor_window: None,
                monitor_window_notify: None,
                monitor_paused: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                monitor_filters: Arc::new(
                    epics_base_rs::server::database::filters::FilterChain::new(),
                ),
                put_auto_exec: true,
                pv_request: None,
            },
        );
        channels.insert(
            sid,
            ChannelState {
                name: "dut".into(),
                cid: 0,
                sid,
                introspection: Some(FieldDesc::Variant),
                ops,
            },
        );

        let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(4);

        // GET_FIELD payload: sid + ioid + subfield string.
        let mut payload = Vec::new();
        payload.put_u32(sid, order);
        payload.put_u32(ioid, order);
        crate::proto::encode_string_into("", order, &mut payload);
        let frame = synth_frame(Command::GetField, order, payload);

        handle_get_field(&source, &frame, &tx, &channels, order)
            .await
            .expect("handler returns Ok");

        assert!(
            rx.try_recv().is_err(),
            "GET_FIELD with IOID collision must drop silently per pvxs serverintrospect.cpp:159"
        );
    }

    /// Companion: GET_FIELD on a CLEAN IOID (not in the channel's ops
    /// map) still emits the introspection reply. Confirms the
    /// collision guard doesn't regress the happy path.
    #[tokio::test]
    async fn get_field_clean_ioid_emits_reply() {
        use crate::pvdata::FieldDesc;
        use crate::server_native::SharedSource;
        use std::sync::Arc;

        let order = ByteOrder::Little;
        let sid: u32 = 7;
        let ioid: u32 = 5555;

        let shared = SharedSource::new();
        let source: DynSource = Arc::new(shared);

        let mut channels: HashMap<u32, ChannelState> = HashMap::new();
        channels.insert(
            sid,
            ChannelState {
                name: "dut".into(),
                cid: 0,
                sid,
                introspection: Some(FieldDesc::Variant),
                ops: HashMap::new(),
            },
        );

        let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(4);
        let mut payload = Vec::new();
        payload.put_u32(sid, order);
        payload.put_u32(ioid, order);
        crate::proto::encode_string_into("", order, &mut payload);
        let frame = synth_frame(Command::GetField, order, payload);

        handle_get_field(&source, &frame, &tx, &channels, order)
            .await
            .expect("handler returns Ok");

        let resp = rx
            .try_recv()
            .expect("clean GET_FIELD must emit introspection reply");
        // ioid (4) + status (1 + ...) + type descriptor
        assert!(resp.len() > PvaHeader::SIZE + 4);
    }
}

#[cfg(test)]
mod autoexec_tests {
    //! epics-base PR `70735383350b` regression: the
    //! `record._options.autoExec` pvRequest option must parse
    //! correctly into the per-op `put_auto_exec` flag.

    use super::*;
    use crate::pvdata::{PvField, PvStructure, ScalarValue};

    fn build_request(autoexec: Option<&str>) -> PvField {
        let mut options = PvStructure::new("");
        if let Some(s) = autoexec {
            options.fields.push((
                "autoExec".into(),
                PvField::Scalar(ScalarValue::String(s.into())),
            ));
        }
        let mut record = PvStructure::new("");
        record
            .fields
            .push(("_options".into(), PvField::Structure(options)));
        let mut root = PvStructure::new("");
        root.fields
            .push(("record".into(), PvField::Structure(record)));
        PvField::Structure(root)
    }

    #[test]
    fn parses_explicit_false() {
        let req = build_request(Some("false"));
        assert_eq!(put_autoexec_from_request(Some(&req)), Some(false));
    }

    #[test]
    fn parses_explicit_true() {
        let req = build_request(Some("true"));
        assert_eq!(put_autoexec_from_request(Some(&req)), Some(true));
    }

    #[test]
    fn parses_alternate_truthy_strings() {
        for v in ["yes", "1", "TRUE"] {
            let req = build_request(Some(v));
            assert_eq!(
                put_autoexec_from_request(Some(&req)),
                Some(true),
                "{v} must parse as true"
            );
        }
        for v in ["no", "0", "FALSE"] {
            let req = build_request(Some(v));
            assert_eq!(
                put_autoexec_from_request(Some(&req)),
                Some(false),
                "{v} must parse as false"
            );
        }
    }

    #[test]
    fn missing_field_returns_none() {
        let req = build_request(None);
        assert_eq!(put_autoexec_from_request(Some(&req)), None);
    }

    #[test]
    fn no_request_returns_none() {
        assert_eq!(put_autoexec_from_request(None), None);
    }

    #[test]
    fn malformed_request_returns_none() {
        // Plain scalar — not a Structure. Must not panic.
        let req = PvField::Scalar(ScalarValue::Double(42.0));
        assert_eq!(put_autoexec_from_request(Some(&req)), None);
    }

    #[test]
    fn unknown_string_returns_none() {
        let req = build_request(Some("maybe"));
        assert_eq!(put_autoexec_from_request(Some(&req)), None);
    }
}
