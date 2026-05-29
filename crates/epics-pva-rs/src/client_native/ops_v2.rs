//! Channel-aware ops with automatic reconnect.
//!
//! These replace the older `ops::*` functions which operated on a one-shot
//! `Connection` with no reconnect logic. The v2 versions take a
//! [`Channel`] and:
//!
//! - GET / PUT / RPC: a single attempt; if the connection dies mid-op the
//!   error bubbles up and the caller decides whether to retry. (Idempotent
//!   ops like GET could in principle be auto-retried, but pvxs prefers to
//!   surface the error so the user can decide.)
//! - MONITOR: re-issues INIT + START on every reconnect transparently. The
//!   `callback` continues firing as long as the channel isn't closed.
//!
//! Pipeline flow control: if `pipeline_size > 0`, the client periodically
//! sends MONITOR_ACK (subcmd `0x80`) to keep the server's send window
//! open. The ACK is sent at the HALF-window mark (`ack_threshold`),
//! matching pvxs's default `ackAt = queueSize/2` — replenishing only at
//! the full window let the server window drain to 0 and stalled ~1 RTT
//! every `pipeline_size` updates.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::time::timeout;
use tracing::debug;

use crate::codec::PvaCodec;
use crate::error::{PvaError, PvaResult};
use crate::proto::{BitSet, ByteOrder, Command, PvaHeader, QosFlags, ReadExt, WriteExt};
use crate::pv_request::{build_pv_request_fields, build_pv_request_value_only};
use crate::pvdata::encode::{encode_pv_field, encode_pv_field_with_bitset, encode_type_desc};
use crate::pvdata::{
    FieldDesc, PvField, PvStructure, ScalarType, ScalarValue, UnionItem, VariantValue,
};

use super::channel::Channel;
use super::decode::{
    Frame, GetFieldResponse, OpResponse, decode_get_field_response, decode_op_response_cached,
};
use super::server_conn::ServerConn;

/// Decode a one-shot GET/PUT/RPC op-response frame, closing the virtual
/// circuit on a wire-decode fault.
///
/// pvxs treats a malformed op-response body (bad cursor / truncated payload,
/// `M.fault()`) — or a wrong op-state — as a connection-level protocol
/// violation and forces `bev.reset()` (`clientget.cpp:456-493`), not a
/// per-operation error. A non-success `Status` is decoded *successfully*
/// (it is data, not a fault) and is surfaced per-op by the caller. Closing
/// here matters because a circuit left alive after a bad frame would carry a
/// corrupted peer / type-cache state into later ops on the same connection.
///
/// The decode is threaded through the connection-scoped `TypeCache`
/// (`server.type_cache()`) so a data-phase `any` / `variant` value whose
/// inner descriptor is a `0xFE <slot>` back-reference resolves against the
/// slot a *prior* frame defined with `0xFD`. pvxs decodes both INIT
/// descriptors and DATA values through the same connection `rxRegistry`
/// (`clientget.cpp:410-451`, `clientmon.cpp:485-552`); decoding DATA with a
/// fresh empty cache here would report a spurious slot miss the moment a
/// peer starts using the descriptor cache.
fn decode_op_or_reset(
    server: &ServerConn,
    frame: &Frame,
    introspection: Option<&FieldDesc>,
) -> PvaResult<OpResponse> {
    let cache = server.type_cache();
    decode_op_response_cached(frame, introspection, &mut cache.lock())
        .inspect_err(|_| server.close())
}

/// Like [`decode_op_or_reset`] but takes the connection `TypeCache` by
/// reference, for INIT callers that already hold it locked-and-reused
/// across the INIT/DATA legs of the same op. Same connection-fatal
/// contract and same cache semantics.
fn decode_op_cached_or_reset(
    server: &ServerConn,
    frame: &Frame,
    introspection: Option<&FieldDesc>,
    type_cache: &mut crate::pvdata::encode::TypeCache,
) -> PvaResult<OpResponse> {
    decode_op_response_cached(frame, introspection, type_cache).inspect_err(|_| server.close())
}

/// GET_FIELD analog: pvxs resets the circuit on a bad GET_FIELD descriptor
/// buffer (`clientintrospect.cpp:115-133`). A non-success `Status` decodes
/// to `Ok` (no introspection) and stays per-op at the caller.
fn decode_get_field_or_reset(server: &ServerConn, frame: &Frame) -> PvaResult<GetFieldResponse> {
    decode_get_field_response(frame).inspect_err(|_| server.close())
}

static NEXT_IOID: AtomicU32 = AtomicU32::new(1);
fn alloc_ioid() -> u32 {
    NEXT_IOID.fetch_add(1, Ordering::Relaxed)
}

/// Default pipeline window for monitors. Tuned to match pvxs.
pub const DEFAULT_PIPELINE_SIZE: u32 = 4;

/// MONITOR_ACK replenishment threshold for a pipeline window of
/// `pipeline_size`. pvxs resolves `ackAt` to `queueSize/2` by default
/// then clamps it to `[1, queueSize]` (`clientmon.cpp:801-808`), so the
/// server's send window is replenished at the HALF-window mark — not
/// when it has fully drained. Acking only at the full window let the
/// window reach 0 and cost ~1 RTT of stall every `pipeline_size`
/// updates under a sustained pipelined monitor. The `ackAny` pvRequest
/// option (a server-chosen count / percent override) is not yet parsed
/// by this client; the half-window default is the common case.
fn ack_threshold(pipeline_size: u32) -> u32 {
    (pipeline_size / 2).max(1)
}

/// Negotiated monitor flow control for one subscription cycle.
///
/// Replaces the bare `pipeline_size: u32` that used to drive three
/// distinct wire decisions at once — whether the MONITOR INIT carries
/// the pipeline bit, the initial `nack` (credit window) trailer value,
/// and the MONITOR_ACK refill cadence. When a caller supplies a custom
/// pvRequest those three must come from the request's
/// `record._options` (pvxs `clientmon.cpp:761-808`), not from the
/// builder default, or the wire `queueSize`/`pipeline` and the client's
/// local accounting disagree. Deriving all three into one struct, once,
/// makes that disagreement unrepresentable.
#[derive(Clone, Copy, Debug)]
pub struct MonitorFlow {
    /// pvxs `op->pipeline`. When false no credit window is negotiated:
    /// INIT carries no pipeline bit / `nack` trailer and no ACK is sent.
    pub pipeline: bool,
    /// pvxs `op->queueSize` — the credit window written as the INIT
    /// `nack` trailer. Only meaningful when `pipeline`.
    pub queue_size: u32,
    /// pvxs `op->ackAt` — refill the server's window after this many
    /// delivered events. Only meaningful when `pipeline`.
    pub ack_at: u32,
}

impl MonitorFlow {
    /// Default-path flow control: the client's configured pipeline
    /// window is the single source of truth (no caller pvRequest to
    /// honor). `pipeline_size == 0` disables pipelining entirely,
    /// matching the pre-`MonitorFlow` `pipeline_size > 0` gate.
    pub fn window(pipeline_size: u32) -> Self {
        if pipeline_size > 0 {
            Self {
                pipeline: true,
                queue_size: pipeline_size,
                ack_at: ack_threshold(pipeline_size),
            }
        } else {
            Self {
                pipeline: false,
                queue_size: 0,
                ack_at: 0,
            }
        }
    }

    /// Derive flow control from a caller-supplied pvRequest's
    /// `record._options`, mirroring pvxs `MonitorBuilder::exec()`
    /// (`clientmon.cpp:761-808`): `pipeline` defaults false; `queueSize`
    /// is honored only when present, parseable, and `> 1`, otherwise the
    /// builder default stands; `ackAny` resolves to a percent of, or an
    /// absolute count within, the window. This is the structural fix for
    /// the custom-request path — the wire `pipeline`/`queueSize`/`nack`
    /// and the ACK cadence now share one origin instead of the wire
    /// coming from the request and the accounting from the builder
    /// default.
    ///
    /// `default_window` is the client's configured pipeline window
    /// (`PvaClientBuilder::pipeline_size`), used as the `queueSize`
    /// fallback the way pvxs uses its `MonitorBuilder` default of 4.
    pub fn from_record_options(record_options: &[(String, String)], default_window: u32) -> Self {
        let get = |key: &str| {
            record_options
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.trim())
        };
        // pvxs `options["pipeline"].as(op->pipeline)` — absent ⟹ false.
        let pipeline = get("pipeline")
            .map(|v| matches!(v.to_ascii_lowercase().as_str(), "true" | "1" | "yes"))
            .unwrap_or(false);
        if !pipeline {
            // No pipeline ⟹ no credit window and no ACK trailer. A
            // requested `queueSize` still travels inside the pvRequest
            // bytes (the server's squash depth) but the client sends no
            // `nack` and never ACKs — pvxs only writes the trailer
            // `if(pipeline)` (`clientmon.cpp:340-348`).
            return Self {
                pipeline: false,
                queue_size: 0,
                ack_at: 0,
            };
        }
        // pvxs `clientmon.cpp:763-773`: `queueSize` honored only when
        // present, parseable, and `Q > 1`; else the builder default. A
        // pipeline window must be `>= 2`, so a 0/1 configured default
        // falls back to the pvxs default of 4.
        let fallback = if default_window > 1 {
            default_window
        } else {
            DEFAULT_PIPELINE_SIZE
        };
        let queue_size = get("queueSize")
            .and_then(|v| v.parse::<u32>().ok())
            .filter(|&q| q > 1)
            .unwrap_or(fallback);
        let ack_at = ack_at_from_request(get("ackAny"), queue_size);
        Self {
            pipeline: true,
            queue_size,
            ack_at,
        }
    }
}

/// pvxs `clientmon.cpp:777-808` — derive the pipeline ACK-refill
/// threshold `ackAt` from a string-valued `record._options.ackAny` and
/// the negotiated `queue_size`. A `"N%"` value is a percent of the
/// window (honored only for `0 < N <= 100`); otherwise an integer
/// count. An absent/unparseable/out-of-range value, or an explicit `0`,
/// resolves to `queue_size / 2`; the result clamps to `[1, queue_size]`.
/// This is the client-string twin of the server's `ack_at_from`
/// (which reads a typed `PvField`); `queue_size` is always `>= 2` here.
fn ack_at_from_request(ack_any: Option<&str>, queue_size: u32) -> u32 {
    let mut ack_at: u32 = 0;
    if let Some(s) = ack_any {
        if let Some(pct) = s.strip_suffix('%') {
            if let Ok(percent) = pct.trim().parse::<f64>() {
                if percent > 0.0 && percent <= 100.0 {
                    ack_at = (percent / 100.0 * queue_size as f64) as u32;
                }
            }
        } else if let Ok(count) = s.parse::<u32>() {
            ack_at = count;
        }
    }
    if ack_at == 0 {
        ack_at = queue_size / 2;
    }
    ack_at.clamp(1, queue_size)
}

/// Drop-guard for the per-IOID router entry.
///
/// **Client-side**: `unregister_ioid` is always called, even on `?`
/// early-returns from inside `op_*` helpers. The remove is idempotent,
/// so an explicit `unregister_ioid` at the success-path tail and the
/// guard's drop-time call cooperate without double-fault.
///
/// **Server-side**: when an op is abandoned mid-flight (caller drops
/// the future, or a long-running monitor handle is dropped without an
/// explicit `stop()`), the server still holds an in-flight operation
/// keyed by `(sid, ioid)`. If `sid` is set, the guard emits a
/// best-effort DESTROY_REQUEST via `try_send` so the server can free
/// that slot. The send is non-blocking — Drop is sync, and we'd rather
/// drop the cleanup frame than block the runtime; the server reaps
/// stranded ops on disconnect anyway.
struct IoidGuard {
    server: Arc<super::server_conn::ServerConn>,
    ioid: u32,
    /// `Some(sid)` when DESTROY_REQUEST should be sent on drop. Cleared
    /// to `None` (via `disarm()`) once the op has explicitly cleaned up,
    /// to avoid emitting a redundant DESTROY after the success-path
    /// destroy has already been sent.
    destroy_sid: Option<u32>,
    /// When true, Drop becomes a no-op: no DESTROY, no router unregister.
    /// Set by `defuse()` for the warm-GET cache path where the caller
    /// intentionally keeps the (sid, ioid) binding alive past op_get's
    /// return so subsequent warm GETs can reuse it.
    defused: bool,
}

impl IoidGuard {
    fn new(server: Arc<super::server_conn::ServerConn>, ioid: u32) -> Self {
        Self {
            server,
            ioid,
            destroy_sid: None,
            defused: false,
        }
    }

    /// Arm the drop-time DESTROY_REQUEST emitter with this `sid`.
    /// Call after the op has been registered server-side so that any
    /// abandonment (caller drops the future / handle) trips the cleanup.
    fn arm_destroy(&mut self, sid: u32) {
        self.destroy_sid = Some(sid);
    }

    /// Disarm the DESTROY_REQUEST emitter — the success path has already
    /// sent its own cleanup, so the guard should only release the
    /// client-side router slot on drop.
    fn disarm(&mut self) {
        self.destroy_sid = None;
    }

    /// Make Drop a complete no-op. Used by the warm-GET cache path,
    /// which transitions the by_ioid entry from TwoShot to Reusable
    /// and intentionally keeps both the server-side ioid binding and
    /// the client-side router slot alive past op_get's return.
    fn defuse(&mut self) {
        self.defused = true;
        self.destroy_sid = None;
    }
}

impl Drop for IoidGuard {
    fn drop(&mut self) {
        if self.defused {
            return;
        }
        if let Some(sid) = self.destroy_sid.take() {
            // Best-effort server-side cleanup. We can't `await`, so we
            // fall back to a non-blocking enqueue and ignore the result.
            // The frame format is identical to what op_get / op_put emit
            // on success, just synthesised here from the cached
            // byte-order. Failure to enqueue is benign: the server
            // reaps stranded ops when the TCP circuit drops.
            let codec = PvaCodec {
                big_endian: matches!(self.server.byte_order, ByteOrder::Big),
            };
            let frame = codec.build_destroy_request(sid, self.ioid);
            let _ = self.server.try_send(frame);
        }
        self.server.unregister_ioid(self.ioid);
    }
}

// ── GET ────────────────────────────────────────────────────────────────

pub async fn op_get(
    channel: &Arc<Channel>,
    fields: &[&str],
    op_timeout: Duration,
) -> PvaResult<(FieldDesc, PvField)> {
    op_get_inner(channel, fields, None, op_timeout).await
}

/// `op_get` variant accepting a pre-built pvRequest blob (bytes
/// produced by [`crate::pv_request::PvRequestExpr::encode`] or one of
/// the `build_pv_request_*` helpers). Lets callers feed
/// `record[pipeline=true,queueSize=N]` etc. through the convenience
/// surface — pvxs `Context::get(name).pvRequest(...)` parity. The raw
/// bytes win over the `fields` path when supplied; pass `None` to
/// fall back to the field-list builder.
pub async fn op_get_raw(
    channel: &Arc<Channel>,
    pv_req: &[u8],
    op_timeout: Duration,
) -> PvaResult<(FieldDesc, PvField)> {
    op_get_inner(channel, &[], Some(pv_req), op_timeout).await
}

/// Bound the search-and-connect phase by the operation's overall
/// timeout. Now that `Channel::ensure_active` has no caller-side
/// timeout for *any* reason (`Initial` and `Reconnect`
/// both stay pending until a SEARCH_RESPONSE arrives — pvxs
/// `Channel::disconnect` parity, the search engine drives recovery
/// indefinitely), one-shot ops like `pvget` / `pvput` / `pvrpc` /
/// `pvconnect` need an outer cap so a permanently-vanished or
/// never-existed server doesn't make the user-facing future hang
/// forever.
///
/// This is the single owner of that cap: it is the ONLY one-shot
/// resolve path. Every one-shot op routes its search-and-connect —
/// including the byte-order pre-read that `context.rs` does to
/// encode a custom `pvRequest` before the op proper — through here,
/// so the invariant "a one-shot op's resolve is bounded by
/// `op_timeout`" holds at every call site. (Previously, the
/// 200 ms `MULTI_SERVER_WINDOW` cap inside `ensure_active` masked
/// those pre-reads; removing it exposed every bare-`ensure_active`
/// one-shot site as an unbounded hang.) `pvmonitor*` loops keep
/// using the bare `ensure_active` because their natural cancel path
/// is `SubscriptionHandle` drop, not a timeout.
pub(crate) async fn ensure_active_with_op_timeout(
    channel: &Arc<Channel>,
    op_timeout: Duration,
) -> PvaResult<(Arc<super::server_conn::ServerConn>, u32)> {
    // Fast path: avoid allocating a timer future if the channel is already active.
    if let Some(active) = channel.try_get_active() {
        return Ok(active);
    }
    match tokio::time::timeout(op_timeout, channel.ensure_active()).await {
        Ok(result) => result,
        Err(_) => Err(PvaError::Timeout),
    }
}

async fn op_get_inner(
    channel: &Arc<Channel>,
    fields: &[&str],
    raw_pv_req: Option<&[u8]>,
    op_timeout: Duration,
) -> PvaResult<(FieldDesc, PvField)> {
    let (server, sid) = ensure_active_with_op_timeout(channel, op_timeout).await?;
    let order = server.byte_order;
    let big_endian = matches!(order, ByteOrder::Big);
    let codec = PvaCodec { big_endian };

    // Warm-GET fast path: skip INIT when we already have a cached
    // (sid, ioid) bound to this channel from a previous default GET
    // (no `fields` filter, no raw pv_request). Cuts the wire cost in
    // half (1 RTT instead of 2) and saves one frame-decode + one
    // type-cache mutex acquisition. Only applies to the default
    // "fetch every field" path because that's the binding the
    // server cached on our behalf last time.
    let is_default_request = raw_pv_req.is_none() && fields.is_empty();
    if is_default_request {
        if let Some(warm) = take_warm_get(channel, &server, sid) {
            // Refill the slot with a fresh oneshot, send GET, await
            // single response. If anything goes wrong we fall through
            // to the cold path which re-establishes the cache.
            let warm_ioid = warm.ioid;
            match try_warm_get(&server, &codec, &warm, op_timeout).await {
                Ok(Some(pv)) => {
                    // Re-cache so the next call also takes the fast path.
                    *channel.cached_get.lock() = Some(warm);
                    return Ok(((*pv.0).clone(), pv.1));
                }
                Ok(None) | Err(_) => {
                    // warm-GET failure abandoned the cached
                    // (sid, ioid) without cleanup. The server's
                    // per-channel op slot for that IOID stayed alive
                    // until TCP close, and the per-circuit Reusable
                    // routing slot lingered in `by_ioid`. Repeated
                    // warm failures could thus push the server's per-
                    // channel op cap. Mirror pvxs `clientget.cpp:
                    // 188-200` — send DESTROY_REQUEST and unregister
                    // the IOID before the cold INIT allocates a new
                    // one.
                    let order = server.byte_order;
                    let dr = codec.build_destroy_request(sid, warm_ioid);
                    let _ = server.send_for_channel(sid, dr).await;
                    server.unregister_ioid(warm_ioid);
                    let _ = order;
                    // Cache stale (server forgot ioid, channel reset, etc.).
                    // Fall through to cold path; do NOT restore the cache
                    // — the next cold success will refill it.
                }
            }
        }
    }

    let ioid = alloc_ioid();

    let pv_req = match raw_pv_req {
        Some(b) => std::borrow::Cow::Borrowed(b),
        None if fields.is_empty() => std::borrow::Cow::Borrowed(sentinel_all_fields()),
        None => std::borrow::Cow::Owned(build_pv_request_fields(fields, big_endian)),
    };

    // TwoShot routing: one oneshot per response (INIT then DATA).
    // Avoids the per-op `unbounded_channel` allocation that used to
    // back the stream-style path; the reader task pops FIFO from the
    // TwoShot VecDeque so first frame → rx_init, second → rx_data.
    let (rx_init, rx_data) = server.register_ioid_twoshot(sid, ioid, Command::Get.code());
    let mut ioid_guard = IoidGuard::new(server.clone(), ioid);
    let cache = server.type_cache();

    // Pipeline: combine INIT + GET frames into a single buffer and
    // send as one channel message. The writer task writes both in one
    // TCP write_all (they're contiguous bytes). The server parses them
    // as two PVA messages by header length. This reduces writer channel
    // hops from 2 to 1 and guarantees both frames land in the same
    // TCP segment (no Nagle delay between them).
    let mut combined = codec.build_get_init(sid, ioid, &pv_req);
    combined.extend_from_slice(&codec.build_get(sid, ioid));
    // Sync send into the unbounded writer queue — no scheduler hop,
    // mirrors CA's `DirectServerWriter::send_frame`.
    server.send_for_channel_sync(sid, combined)?;

    // Receive INIT response
    let init_frame = await_oneshot_frame(rx_init, op_timeout).await?;
    let init = match decode_op_cached_or_reset(&server, &init_frame, None, &mut cache.lock())? {
        OpResponse::Init(i) => i,
        other => {
            // Wrong response kind for this op step == impossible op state.
            // pvxs `M.fault()`s and `bev.reset()`s (clientget.cpp:456-493).
            server.close();
            server.unregister_ioid(ioid);
            return Err(PvaError::Protocol(format!(
                "expected GET INIT, got {other:?}"
            )));
        }
    };
    if !init.status.is_success() {
        server.unregister_ioid(ioid);
        return Err(PvaError::Protocol(format!(
            "GET INIT failed: {:?}",
            init.status
        )));
    }
    ioid_guard.arm_destroy(sid);
    let intro = init.introspection;

    // Receive DATA response (already sent, just waiting for the reply)
    let data_frame = await_oneshot_frame(rx_data, op_timeout).await?;
    let intro_arc = Arc::new(intro);
    let result = match decode_op_or_reset(&server, &data_frame, Some(&intro_arc))? {
        OpResponse::Data(d) => {
            if d.status.is_success() {
                Ok(((*intro_arc).clone(), d.value))
            } else {
                Err(PvaError::Protocol(format!("GET data: {:?}", d.status)))
            }
        }
        // a data-phase failure now arrives as a status-only
        // reply (server echoes the request data subcmd, no bitset/value),
        // so it decodes to OpResponse::Status. Surface the server status
        // instead of mislabelling it "expected GET data, got Status".
        OpResponse::Status(s) => Err(PvaError::Protocol(format!("GET data: {:?}", s.status))),
        other => {
            // Wrong response kind for the GET data step == impossible op
            // state → connection-fatal (pvxs clientget.cpp:456-493).
            server.close();
            Err(PvaError::Protocol(format!(
                "expected GET data, got {other:?}"
            )))
        }
    };

    // Cache (sid, ioid, intro) so the next default GET on this
    // channel can take the warm path. Replace the TwoShot slot with
    // a Reusable slot the warm path can refill, and defuse the
    // IoidGuard so its Drop does NOT unregister + DESTROY — the
    // server keeps the binding alive for our reuse.
    if is_default_request && result.is_ok() {
        let slot = server.register_ioid_reusable(sid, ioid, Command::Get.code());
        *channel.cached_get.lock() = Some(super::channel::CachedGet {
            server: Arc::downgrade(&server),
            sid,
            ioid,
            intro: intro_arc,
            slot,
        });
        ioid_guard.defuse();
    } else {
        ioid_guard.disarm();
        // Fire-and-forget cleanup — try_send avoids awaiting the
        // writer task, saving one channel hop (~3-5µs).
        let destroy = codec.build_destroy_request(sid, ioid);
        server.try_send(destroy);
        server.unregister_ioid(ioid);
    }
    result
}

/// Take the cached warm-GET state if it's still bound to the same
/// (server, sid). Lazy invalidation: a stale cache (different sid /
/// dropped server / disconnected) silently returns None and the
/// caller falls through to the cold path.
fn take_warm_get(
    channel: &Arc<Channel>,
    server: &Arc<super::server_conn::ServerConn>,
    sid: u32,
) -> Option<super::channel::CachedGet> {
    let mut guard = channel.cached_get.lock();
    let cached = guard.take()?;
    let cached_server = cached.server.upgrade()?;
    if Arc::ptr_eq(&cached_server, server) && cached.sid == sid && server.is_alive() {
        Some(cached)
    } else {
        // Wrong server / sid — drop. Reader's by_ioid entry will be
        // GC'd when the connection tears down.
        None
    }
}

/// Warm-GET fast path: send GET only (no INIT), await single response,
/// decode using cached intro. Returns Ok(None) if the server forgot
/// the ioid (replied with an error status), Ok(Some) on success, Err
/// on transport / timeout failures.
async fn try_warm_get(
    server: &Arc<super::server_conn::ServerConn>,
    codec: &PvaCodec,
    warm: &super::channel::CachedGet,
    op_timeout: Duration,
) -> PvaResult<Option<(Arc<FieldDesc>, PvField)>> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    *warm.slot.lock() = Some(tx);
    let frame = codec.build_get(warm.sid, warm.ioid);
    if server.send_for_channel_sync(warm.sid, frame).is_err() {
        warm.slot.lock().take();
        return Err(PvaError::Protocol("warm GET send failed".into()));
    }
    let data_frame = await_oneshot_frame(rx, op_timeout).await?;
    match decode_op_or_reset(server, &data_frame, Some(&warm.intro))? {
        OpResponse::Data(d) => {
            if d.status.is_success() {
                Ok(Some((warm.intro.clone(), d.value)))
            } else {
                // Server rejected — likely lost the binding (channel
                // close / GC). Caller should retry cold.
                Ok(None)
            }
        }
        _ => Ok(None),
    }
}

// ── GET_FIELD (introspection only) ────────────────────────────────────

/// Fetch the channel's introspection (FieldDesc) without transferring
/// any value. pvxs `Context::info(name)` parity. Much cheaper than a
/// full GET for large PVs (NTNDArray, multi-MiB arrays) since the
/// server replies with descriptor bytes only — no value encoding,
/// no payload bandwidth proportional to the PV size.
///
/// `subfield` (typically the empty string) selects a sub-tree of the
/// channel's structure; pass "" for the root-level introspection.
pub async fn op_get_field(
    channel: &Arc<Channel>,
    subfield: &str,
    op_timeout: Duration,
) -> PvaResult<FieldDesc> {
    let (server, sid) = ensure_active_with_op_timeout(channel, op_timeout).await?;
    let order = server.byte_order;
    let big_endian = matches!(order, ByteOrder::Big);
    let codec = PvaCodec { big_endian };
    let ioid = alloc_ioid();
    let mut stream = server.register_ioid_stream(sid, ioid, Command::GetField.code());
    let _ioid_guard = IoidGuard::new(server.clone(), ioid);

    let req = codec.build_get_field(sid, ioid, subfield);
    let send_result = server.send_for_channel(sid, req).await;
    if send_result.is_err() {
        server.unregister_ioid(ioid);
        return Err(PvaError::Protocol("GET_FIELD send failed".into()));
    }
    let frame = await_frame(&mut stream, op_timeout).await;
    server.unregister_ioid(ioid);
    let frame = frame?;
    let resp = decode_get_field_or_reset(&server, &frame)?;
    if !resp.status.is_success() {
        return Err(PvaError::Protocol(format!(
            "GET_FIELD failed: {:?}",
            resp.status
        )));
    }
    resp.introspection.ok_or_else(|| {
        PvaError::Protocol("GET_FIELD: no introspection in successful response".into())
    })
}

// ── PUT ────────────────────────────────────────────────────────────────

pub async fn op_put(
    channel: &Arc<Channel>,
    value_str: &str,
    op_timeout: Duration,
) -> PvaResult<()> {
    op_put_inner(channel, value_str, None, op_timeout).await
}

/// `op_put` variant accepting a pre-built pvRequest blob. Lets
/// callers thread `record[process=true]` (RPC-like blocking puts) or
/// custom field-mask selections through. Bytes typically built via
/// [`crate::pv_request::PvRequestBuilder::build`] +
/// [`crate::pv_request::PvRequestExpr::encode`]. pvxs
/// `Context::put(name).pvRequest(...)` parity.
pub async fn op_put_raw(
    channel: &Arc<Channel>,
    pv_req: &[u8],
    value_str: &str,
    op_timeout: Duration,
) -> PvaResult<()> {
    op_put_inner(channel, value_str, Some(pv_req), op_timeout).await
}

/// PUT a single dotted-path field of the channel's structure (e.g.
/// `"alarm.severity"`, `"value"`, `"display.units"`). pvxs
/// `PutBuilder::set("path", val)` parity. Server receives a value
/// where only `field_path` carries the parsed string and every other
/// field is a default; the changed bitset has only the path's bit
/// set so the server applies just that one field.
///
/// pvRequest is forced to `field(<path>)` so the server INIT
/// negotiation matches the field layout we'll send.
pub async fn op_put_field(
    channel: &Arc<Channel>,
    field_path: &str,
    value_str: &str,
    op_timeout: Duration,
) -> PvaResult<()> {
    let (server, sid) = ensure_active_with_op_timeout(channel, op_timeout).await?;
    let order = server.byte_order;
    let big_endian = matches!(order, ByteOrder::Big);
    let codec = PvaCodec { big_endian };
    let ioid = alloc_ioid();

    // pvRequest selects exactly the target field so server-side bitset
    // bookkeeping aligns with the descriptor we'll get back.
    let pv_req = if field_path.is_empty() {
        std::borrow::Cow::Borrowed(sentinel_all_fields())
    } else {
        std::borrow::Cow::Owned(build_pv_request_fields(&[field_path], big_endian))
    };
    let mut stream = server.register_ioid_stream(sid, ioid, Command::Put.code());
    let mut ioid_guard = IoidGuard::new(server.clone(), ioid);
    let cache = server.type_cache();

    let init_req = codec.build_put_init(sid, ioid, &pv_req);
    server.send_for_channel(sid, init_req).await?;
    let init_frame = await_frame(&mut stream, op_timeout).await?;
    let init = match decode_op_cached_or_reset(&server, &init_frame, None, &mut cache.lock())? {
        OpResponse::Init(i) => i,
        other => {
            // Wrong response kind for this op step == impossible op state.
            // pvxs `M.fault()`s and `bev.reset()`s (clientget.cpp:456-493).
            server.close();
            server.unregister_ioid(ioid);
            return Err(PvaError::Protocol(format!(
                "expected PUT INIT, got {other:?}"
            )));
        }
    };
    if !init.status.is_success() {
        server.unregister_ioid(ioid);
        return Err(PvaError::Protocol(format!(
            "PUT INIT failed: {:?}",
            init.status
        )));
    }
    ioid_guard.arm_destroy(sid);
    let intro = init.introspection;

    let parts: Vec<&str> = field_path.split('.').filter(|s| !s.is_empty()).collect();
    let value = build_put_value_for_path(&intro, &parts, value_str)?;
    let bit = intro.bit_for_path(field_path).ok_or_else(|| {
        PvaError::InvalidValue(format!(
            "field path '{field_path}' not present in introspection"
        ))
    })?;

    let mut payload = Vec::new();
    payload.put_u32(sid, order);
    payload.put_u32(ioid, order);
    payload.put_u8(0x00);
    let mut changed = BitSet::new();
    changed.set(bit);
    changed.write_into(order, &mut payload);
    // pvxs `from_wire_valid` (serverget.cpp:451) decodes a BitSet delta —
    // only the fields whose bit is set. Encode consistently so a desync
    // does not corrupt the server-side decode.
    encode_pv_field_with_bitset(&value, &intro, &changed, 0, order, &mut payload);
    let header = PvaHeader::application(false, order, Command::Put.code(), payload.len() as u32);
    let mut frame = Vec::new();
    header.write_into(&mut frame);
    frame.extend_from_slice(&payload);
    server.send_for_channel(sid, frame).await?;

    let done_frame = await_frame(&mut stream, op_timeout).await?;
    let result = match decode_op_or_reset(&server, &done_frame, Some(&intro))? {
        OpResponse::Status(s) => {
            if s.status.is_success() {
                Ok(())
            } else {
                Err(PvaError::Protocol(format!("PUT failed: {:?}", s.status)))
            }
        }
        other => {
            // Wrong response kind for the PUT completion step == impossible
            // op state → connection-fatal (pvxs clientget.cpp:456-493).
            server.close();
            Err(PvaError::Protocol(format!(
                "expected PUT done, got {other:?}"
            )))
        }
    };

    ioid_guard.disarm();
    let destroy = codec.build_destroy_request(sid, ioid);
    let _ = server.send_for_channel(sid, destroy).await;
    server.unregister_ioid(ioid);
    result
}

/// Clone the sub-value at a dotted path out of a value tree.
fn value_at_path(root: &PvField, parts: &[&str]) -> Option<PvField> {
    match parts.split_first() {
        None => Some(root.clone()),
        Some((head, tail)) => match root {
            PvField::Structure(s) => s.get_field(head).and_then(|c| value_at_path(c, tail)),
            _ => None,
        },
    }
}

/// Assign `leaf` at a dotted path inside a value tree (the structure
/// must already contain the path — it does, since the accumulator is
/// built from the prototype default). No-op if the path is absent.
fn assign_at_path(root: &mut PvField, parts: &[&str], leaf: PvField) {
    match parts.split_first() {
        None => *root = leaf,
        Some((head, tail)) => {
            if let PvField::Structure(s) = root {
                if let Some(child) = s.get_field_mut(head) {
                    assign_at_path(child, tail, leaf);
                }
            }
        }
    }
}

/// PUT multiple `field=value` assignments as a single delta. Mirrors
/// pvxs `pvxput` (`tools/put.cpp:115-134`): build from the channel
/// prototype, assign every requested field by dotted path, and mark
/// only the assigned fields in the changed BitSet. `pv_req_override`
/// supplies the INIT pvRequest (e.g. `-r record[process=true]`); when
/// `None`, the request selects exactly the assigned field paths.
///
/// one prototype-based PUT, not one round-trip per field and
/// not a single string concatenated into `.value`.
pub async fn op_put_fields(
    channel: &Arc<Channel>,
    assignments: &[(String, String)],
    pv_req_override: Option<&[u8]>,
    op_timeout: Duration,
) -> PvaResult<()> {
    if assignments.is_empty() {
        return Err(PvaError::InvalidValue("no field assignments".into()));
    }
    let (server, sid) = ensure_active_with_op_timeout(channel, op_timeout).await?;
    let order = server.byte_order;
    let big_endian = matches!(order, ByteOrder::Big);
    let codec = PvaCodec { big_endian };
    let ioid = alloc_ioid();

    let derived_req;
    let pv_req: &[u8] = match pv_req_override {
        Some(bytes) => bytes,
        None => {
            let paths: Vec<&str> = assignments.iter().map(|(p, _)| p.as_str()).collect();
            derived_req = build_pv_request_fields(&paths, big_endian);
            &derived_req
        }
    };

    let mut stream = server.register_ioid_stream(sid, ioid, Command::Put.code());
    let mut ioid_guard = IoidGuard::new(server.clone(), ioid);
    let cache = server.type_cache();

    let init_req = codec.build_put_init(sid, ioid, pv_req);
    server.send_for_channel(sid, init_req).await?;
    let init_frame = await_frame(&mut stream, op_timeout).await?;
    let init = match decode_op_cached_or_reset(&server, &init_frame, None, &mut cache.lock())? {
        OpResponse::Init(i) => i,
        other => {
            // Wrong response kind for this op step == impossible op state.
            // pvxs `M.fault()`s and `bev.reset()`s (clientget.cpp:456-493).
            server.close();
            server.unregister_ioid(ioid);
            return Err(PvaError::Protocol(format!(
                "expected PUT INIT, got {other:?}"
            )));
        }
    };
    if !init.status.is_success() {
        server.unregister_ioid(ioid);
        return Err(PvaError::Protocol(format!(
            "PUT INIT failed: {:?}",
            init.status
        )));
    }
    ioid_guard.arm_destroy(sid);
    let intro = init.introspection;

    // Build the delta from the prototype default, then overwrite just
    // the assigned leaves and mark just their bits.
    let mut value = crate::pvdata::encode::default_value_for(&intro);
    let mut changed = BitSet::new();
    for (path, value_str) in assignments {
        let parts: Vec<&str> = path.split('.').filter(|s| !s.is_empty()).collect();
        let bit = intro.bit_for_path(path).ok_or_else(|| {
            PvaError::InvalidValue(format!("field path '{path}' not present in introspection"))
        })?;
        // Reuse the single-path builder (parse + tree shape), then lift
        // just this path's leaf into the shared accumulator.
        let one = build_put_value_for_path(&intro, &parts, value_str)?;
        let leaf = value_at_path(&one, &parts)
            .ok_or_else(|| PvaError::InvalidValue(format!("could not build field '{path}'")))?;
        assign_at_path(&mut value, &parts, leaf);
        changed.set(bit);
    }

    let mut payload = Vec::new();
    payload.put_u32(sid, order);
    payload.put_u32(ioid, order);
    payload.put_u8(0x00);
    changed.write_into(order, &mut payload);
    encode_pv_field_with_bitset(&value, &intro, &changed, 0, order, &mut payload);
    let header = PvaHeader::application(false, order, Command::Put.code(), payload.len() as u32);
    let mut frame = Vec::new();
    header.write_into(&mut frame);
    frame.extend_from_slice(&payload);
    server.send_for_channel(sid, frame).await?;

    let done_frame = await_frame(&mut stream, op_timeout).await?;
    let result = match decode_op_or_reset(&server, &done_frame, Some(&intro))? {
        OpResponse::Status(s) => {
            if s.status.is_success() {
                Ok(())
            } else {
                Err(PvaError::Protocol(format!("PUT failed: {:?}", s.status)))
            }
        }
        other => {
            // Wrong response kind for the PUT completion step == impossible
            // op state → connection-fatal (pvxs clientget.cpp:456-493).
            server.close();
            Err(PvaError::Protocol(format!(
                "expected PUT done, got {other:?}"
            )))
        }
    };

    ioid_guard.disarm();
    let destroy = codec.build_destroy_request(sid, ioid);
    let _ = server.send_for_channel(sid, destroy).await;
    server.unregister_ioid(ioid);
    result
}

/// PUT a single dotted-path field using a caller-provided pvRequest.
/// Like [`op_put_field`] but INIT uses `pv_req` bytes supplied by the
/// caller (typically `field() record[process=..,block=..]`) instead of
/// a derived `field(<path>)` selector. The DATA phase still targets
/// `field_path` exclusively. `field_path` must be non-empty.
///
/// pvxs parity: `pvalink_channel.cpp:31-38` (putReq template carries
/// record options) + `linkBuildPut:138` (field targeting via
/// `top[fieldName]`).
pub async fn op_put_field_with_request(
    channel: &Arc<Channel>,
    field_path: &str,
    pv_req: &[u8],
    value_str: &str,
    op_timeout: Duration,
) -> PvaResult<()> {
    let (server, sid) = ensure_active_with_op_timeout(channel, op_timeout).await?;
    let order = server.byte_order;
    let big_endian = matches!(order, ByteOrder::Big);
    let codec = PvaCodec { big_endian };
    let ioid = alloc_ioid();

    let mut stream = server.register_ioid_stream(sid, ioid, Command::Put.code());
    let mut ioid_guard = IoidGuard::new(server.clone(), ioid);
    let cache = server.type_cache();

    let init_req = codec.build_put_init(sid, ioid, pv_req);
    server.send_for_channel(sid, init_req).await?;
    let init_frame = await_frame(&mut stream, op_timeout).await?;
    let init = match decode_op_cached_or_reset(&server, &init_frame, None, &mut cache.lock())? {
        OpResponse::Init(i) => i,
        other => {
            // Wrong response kind for this op step == impossible op state.
            // pvxs `M.fault()`s and `bev.reset()`s (clientget.cpp:456-493).
            server.close();
            server.unregister_ioid(ioid);
            return Err(PvaError::Protocol(format!(
                "expected PUT INIT, got {other:?}"
            )));
        }
    };
    if !init.status.is_success() {
        server.unregister_ioid(ioid);
        return Err(PvaError::Protocol(format!(
            "PUT INIT failed: {:?}",
            init.status
        )));
    }
    ioid_guard.arm_destroy(sid);
    let intro = init.introspection;

    let parts: Vec<&str> = field_path.split('.').filter(|s| !s.is_empty()).collect();
    let value = build_put_value_for_path(&intro, &parts, value_str)?;
    let bit = intro.bit_for_path(field_path).ok_or_else(|| {
        PvaError::InvalidValue(format!(
            "field path '{field_path}' not present in introspection"
        ))
    })?;

    let mut payload = Vec::new();
    payload.put_u32(sid, order);
    payload.put_u32(ioid, order);
    payload.put_u8(0x00);
    let mut changed = BitSet::new();
    changed.set(bit);
    changed.write_into(order, &mut payload);
    encode_pv_field_with_bitset(&value, &intro, &changed, 0, order, &mut payload);
    let header = PvaHeader::application(false, order, Command::Put.code(), payload.len() as u32);
    let mut frame = Vec::new();
    header.write_into(&mut frame);
    frame.extend_from_slice(&payload);
    server.send_for_channel(sid, frame).await?;

    let done_frame = await_frame(&mut stream, op_timeout).await?;
    let result = match decode_op_or_reset(&server, &done_frame, Some(&intro))? {
        OpResponse::Status(s) => {
            if s.status.is_success() {
                Ok(())
            } else {
                Err(PvaError::Protocol(format!("PUT failed: {:?}", s.status)))
            }
        }
        other => {
            // Wrong response kind for the PUT completion step == impossible
            // op state → connection-fatal (pvxs clientget.cpp:456-493).
            server.close();
            Err(PvaError::Protocol(format!(
                "expected PUT done, got {other:?}"
            )))
        }
    };

    ioid_guard.disarm();
    let destroy = codec.build_destroy_request(sid, ioid);
    let _ = server.send_for_channel(sid, destroy).await;
    server.unregister_ioid(ioid);
    result
}

/// PUT a pre-built [`PvField`] into a single dotted-path field of the
/// channel's structure, using a caller-provided pvRequest. Combines
/// the typed-value path of [`op_put_value_raw`] with the
/// field-targeting of [`op_put_field_with_request`]: the typed
/// `value` is placed at `field_path` and only that path's bit is set
/// in the changed BitSet.
///
/// `field_path` must be non-empty. If the introspected leaf at
/// `field_path` is itself an NT-style structure with a `value`
/// sub-field, the typed `value` is placed at `<field_path>.value` —
/// mirroring pvxs `linkBuildPut` (`pvalink_channel.cpp:138-143`):
/// `auto value(top[fieldName]); if(struct) value = value["value"]`.
///
/// pvxs parity: `pvalink_channel.cpp:127-180` typed array/scalar PUT
/// into the link's `fieldName` target.
pub async fn op_put_value_field_with_request(
    channel: &Arc<Channel>,
    field_path: &str,
    pv_req: &[u8],
    value: &PvField,
    op_timeout: Duration,
) -> PvaResult<()> {
    let (server, sid) = ensure_active_with_op_timeout(channel, op_timeout).await?;
    let order = server.byte_order;
    let big_endian = matches!(order, ByteOrder::Big);
    let codec = PvaCodec { big_endian };
    let ioid = alloc_ioid();

    let mut stream = server.register_ioid_stream(sid, ioid, Command::Put.code());
    let mut ioid_guard = IoidGuard::new(server.clone(), ioid);
    let cache = server.type_cache();

    let init_req = codec.build_put_init(sid, ioid, pv_req);
    server.send_for_channel(sid, init_req).await?;
    let init_frame = await_frame(&mut stream, op_timeout).await?;
    let init = match decode_op_cached_or_reset(&server, &init_frame, None, &mut cache.lock())? {
        OpResponse::Init(i) => i,
        other => {
            // Wrong response kind for this op step == impossible op state.
            // pvxs `M.fault()`s and `bev.reset()`s (clientget.cpp:456-493).
            server.close();
            server.unregister_ioid(ioid);
            return Err(PvaError::Protocol(format!(
                "expected PUT INIT, got {other:?}"
            )));
        }
    };
    if !init.status.is_success() {
        server.unregister_ioid(ioid);
        return Err(PvaError::Protocol(format!(
            "PUT INIT failed: {:?}",
            init.status
        )));
    }
    ioid_guard.arm_destroy(sid);
    let intro = init.introspection;

    // Resolve the effective leaf path: if the introspected field at
    // `field_path` is an NT-style struct carrying a `value` child,
    // drill one level deeper so the typed value lands on the actual
    // scalar/array leaf (pvxs `linkBuildPut:138-143`).
    let effective_path = effective_typed_put_path(&intro, field_path);
    let parts: Vec<&str> = effective_path
        .split('.')
        .filter(|s| !s.is_empty())
        .collect();
    let put_value = build_put_value_typed_for_path(&intro, &parts, value)?;
    let bit = intro.bit_for_path(&effective_path).ok_or_else(|| {
        PvaError::InvalidValue(format!(
            "field path '{effective_path}' not present in introspection"
        ))
    })?;

    let mut payload = Vec::new();
    payload.put_u32(sid, order);
    payload.put_u32(ioid, order);
    payload.put_u8(0x00);
    let mut changed = BitSet::new();
    changed.set(bit);
    changed.write_into(order, &mut payload);
    encode_pv_field_with_bitset(&put_value, &intro, &changed, 0, order, &mut payload);
    let header = PvaHeader::application(false, order, Command::Put.code(), payload.len() as u32);
    let mut frame = Vec::new();
    header.write_into(&mut frame);
    frame.extend_from_slice(&payload);
    server.send_for_channel(sid, frame).await?;

    let done_frame = await_frame(&mut stream, op_timeout).await?;
    let result = match decode_op_or_reset(&server, &done_frame, Some(&intro))? {
        OpResponse::Status(s) => {
            if s.status.is_success() {
                Ok(())
            } else {
                Err(PvaError::Protocol(format!("PUT failed: {:?}", s.status)))
            }
        }
        other => {
            // Wrong response kind for the PUT completion step == impossible
            // op state → connection-fatal (pvxs clientget.cpp:456-493).
            server.close();
            Err(PvaError::Protocol(format!(
                "expected PUT done, got {other:?}"
            )))
        }
    };

    ioid_guard.disarm();
    let destroy = codec.build_destroy_request(sid, ioid);
    let _ = server.send_for_channel(sid, destroy).await;
    server.unregister_ioid(ioid);
    result
}

/// Resolve the effective leaf path for a typed field-targeted PUT.
/// If the field at `field_path` is an NT-style structure that has a
/// `value` sub-field, returns `<field_path>.value`; otherwise returns
/// `field_path` unchanged. Mirrors pvxs `linkBuildPut:138-143`.
fn effective_typed_put_path(intro: &FieldDesc, field_path: &str) -> String {
    let parts: Vec<&str> = field_path.split('.').filter(|s| !s.is_empty()).collect();
    let mut cursor = intro;
    for seg in &parts {
        match cursor {
            FieldDesc::Structure { fields, .. } => match fields.iter().find(|(n, _)| n == seg) {
                Some((_, child)) => cursor = child,
                None => return field_path.to_string(),
            },
            _ => return field_path.to_string(),
        }
    }
    if let FieldDesc::Structure { fields, .. } = cursor {
        if fields.iter().any(|(n, _)| n == "value") {
            return format!("{field_path}.value");
        }
    }
    field_path.to_string()
}

/// Like [`build_put_value_for_path`] but places a pre-built typed
/// [`PvField`] at the leaf instead of parsing a string. Every field
/// off the target path is filled with its default value so the
/// encoded structure matches the introspection layout; only the
/// path's bit is set in the changed BitSet by the caller.
fn build_put_value_typed_for_path(
    desc: &FieldDesc,
    field_path: &[&str],
    value: &PvField,
) -> PvaResult<PvField> {
    if field_path.is_empty() {
        return Ok(value.clone());
    }
    match desc {
        FieldDesc::Structure { fields, struct_id } => {
            let head = field_path[0];
            let tail = &field_path[1..];
            if !fields.iter().any(|(n, _)| n == head) {
                return Err(PvaError::InvalidValue(format!(
                    "field '{head}' not present in target structure"
                )));
            }
            let mut s = PvStructure::new(struct_id);
            for (name, child) in fields {
                if name == head {
                    s.fields.push((
                        name.clone(),
                        build_put_value_typed_for_path(child, tail, value)?,
                    ));
                } else {
                    s.fields.push((
                        name.clone(),
                        crate::pvdata::encode::default_value_for(child),
                    ));
                }
            }
            Ok(PvField::Structure(s))
        }
        _ => Err(PvaError::InvalidValue(format!(
            "cannot navigate path through {desc} (remaining: {field_path:?})"
        ))),
    }
}

/// Coerce a typed PUT `value` to the shape `intro` expects before
/// encoding.
///
/// `op_put_value` / `op_put_value_raw` encode `value` against the
/// server's introspection and target the `value` bit. Callers pass
/// either a full NT structure (`pvput_typed` via
/// `TypedNT::to_pv_field`) or a *bare leaf* value — pvalink OUT
/// arrays go through `crate::convert::epics_to_pv_field`, which
/// returns a bare `PvField::ScalarArray` / `PvField::Scalar`.
///
/// `encode_pv_field` has no `(Structure desc, bare-leaf value)` arm,
/// so a bare leaf encoded directly against a `Structure` intro emits
/// zero bytes and the server applies an empty value. When `intro` is
/// a `Structure` with a `value` sub-field and `value` is not itself
/// a `Structure`, this wraps the bare leaf at the `value` path so
/// the encoder sees a structurally-matching value. A `value` that is
/// already a `Structure`, or an `intro` that is itself a bare leaf,
/// passes through unchanged.
fn coerce_typed_put_value(intro: &FieldDesc, value: &PvField) -> PvaResult<PvField> {
    match (intro, value) {
        (FieldDesc::Structure { fields, .. }, v) if !matches!(v, PvField::Structure(_)) => {
            if fields.iter().any(|(n, _)| n == "value") {
                build_put_value_typed_for_path(intro, &["value"], value)
            } else {
                // No `value` sub-field to target — leave as-is; the
                // encoder mismatch will surface as a clear "PUT
                // failed" rather than a silent empty write.
                Ok(value.clone())
            }
        }
        _ => Ok(value.clone()),
    }
}

/// PUT a pre-built [`PvField`] (typed-NT path). Skips the
/// string-form round-trip used by [`op_put`] / [`op_put_raw`] —
/// `value` is encoded directly against the server-supplied
/// introspection. The caller's typed-NT shape MUST match the
/// server's introspection at the wire level; mismatch surfaces as
/// the standard "PUT failed" status from the server.
///
/// Used by [`crate::client_native::context::PvaClient::pvput_typed`].
pub async fn op_put_value(
    channel: &Arc<Channel>,
    value: &PvField,
    op_timeout: Duration,
) -> PvaResult<()> {
    let (server, sid) = ensure_active_with_op_timeout(channel, op_timeout).await?;
    let order = server.byte_order;
    let big_endian = matches!(order, ByteOrder::Big);
    let codec = PvaCodec { big_endian };
    let ioid = alloc_ioid();

    let pv_req = build_pv_request_value_only(big_endian);
    let mut stream = server.register_ioid_stream(sid, ioid, Command::Put.code());
    let mut ioid_guard = IoidGuard::new(server.clone(), ioid);
    let cache = server.type_cache();

    let init_req = codec.build_put_init(sid, ioid, &pv_req);
    server.send_for_channel(sid, init_req).await?;
    let init_frame = await_frame(&mut stream, op_timeout).await?;
    let init = match decode_op_cached_or_reset(&server, &init_frame, None, &mut cache.lock())? {
        OpResponse::Init(i) => i,
        other => {
            // Wrong response kind for this op step == impossible op state.
            // pvxs `M.fault()`s and `bev.reset()`s (clientget.cpp:456-493).
            server.close();
            return Err(PvaError::Protocol(format!(
                "expected PUT INIT, got {other:?}"
            )));
        }
    };
    if !init.status.is_success() {
        return Err(PvaError::Protocol(format!(
            "PUT INIT failed: {:?}",
            init.status
        )));
    }
    ioid_guard.arm_destroy(sid);
    let intro = init.introspection;

    let mut payload = Vec::new();
    payload.put_u32(sid, order);
    payload.put_u32(ioid, order);
    payload.put_u8(0x00);
    let mut changed = BitSet::new();
    if let Some(bit) = intro.bit_for_path("value") {
        changed.set(bit);
    } else {
        changed.set(0);
    }
    changed.write_into(order, &mut payload);
    // pvxs `from_wire_valid` (serverget.cpp:451) decodes a BitSet delta —
    // only the fields whose bit is set. Encode consistently.
    // Wrap a bare-leaf `value` at the `value` path when `intro` is a
    // structure (pvalink OUT arrays arrive as a bare
    // `ScalarArray`, which would otherwise encode to zero bytes).
    let put_value = coerce_typed_put_value(&intro, value)?;
    encode_pv_field_with_bitset(&put_value, &intro, &changed, 0, order, &mut payload);
    let header = PvaHeader::application(false, order, Command::Put.code(), payload.len() as u32);
    let mut frame = Vec::new();
    header.write_into(&mut frame);
    frame.extend_from_slice(&payload);
    server.send_for_channel(sid, frame).await?;

    let done_frame = await_frame(&mut stream, op_timeout).await?;
    let result = match decode_op_or_reset(&server, &done_frame, Some(&intro))? {
        OpResponse::Status(s) => {
            if s.status.is_success() {
                Ok(())
            } else {
                Err(PvaError::Protocol(format!("PUT failed: {:?}", s.status)))
            }
        }
        other => {
            // Wrong response kind for the PUT completion step == impossible
            // op state → connection-fatal (pvxs clientget.cpp:456-493).
            server.close();
            Err(PvaError::Protocol(format!(
                "expected PUT done, got {other:?}"
            )))
        }
    };

    ioid_guard.disarm();
    let destroy = codec.build_destroy_request(sid, ioid);
    let _ = server.send_for_channel(sid, destroy).await;
    result
}

/// PUT a pre-built [`PvField`] with a caller-provided pvRequest.
/// Like [`op_put_value`] but INIT uses the caller's `pv_req` bytes
/// (for `record._options` like `process` / `block`) instead of the
/// default `field(value)` selector. DATA still targets the `"value"`
/// bit. pvxs `pvalink_channel.cpp:268` parity for typed OUT arrays.
pub async fn op_put_value_raw(
    channel: &Arc<Channel>,
    pv_req: &[u8],
    value: &PvField,
    op_timeout: Duration,
) -> PvaResult<()> {
    let (server, sid) = ensure_active_with_op_timeout(channel, op_timeout).await?;
    let order = server.byte_order;
    let big_endian = matches!(order, ByteOrder::Big);
    let codec = PvaCodec { big_endian };
    let ioid = alloc_ioid();

    let mut stream = server.register_ioid_stream(sid, ioid, Command::Put.code());
    let mut ioid_guard = IoidGuard::new(server.clone(), ioid);
    let cache = server.type_cache();

    let init_req = codec.build_put_init(sid, ioid, pv_req);
    server.send_for_channel(sid, init_req).await?;
    let init_frame = await_frame(&mut stream, op_timeout).await?;
    let init = match decode_op_cached_or_reset(&server, &init_frame, None, &mut cache.lock())? {
        OpResponse::Init(i) => i,
        other => {
            // Wrong response kind for this op step == impossible op state.
            // pvxs `M.fault()`s and `bev.reset()`s (clientget.cpp:456-493).
            server.close();
            return Err(PvaError::Protocol(format!(
                "expected PUT INIT, got {other:?}"
            )));
        }
    };
    if !init.status.is_success() {
        return Err(PvaError::Protocol(format!(
            "PUT INIT failed: {:?}",
            init.status
        )));
    }
    ioid_guard.arm_destroy(sid);
    let intro = init.introspection;

    let mut payload = Vec::new();
    payload.put_u32(sid, order);
    payload.put_u32(ioid, order);
    payload.put_u8(0x00);
    let mut changed = BitSet::new();
    if let Some(bit) = intro.bit_for_path("value") {
        changed.set(bit);
    } else {
        changed.set(0);
    }
    changed.write_into(order, &mut payload);
    // wrap a bare-leaf `value` (pvalink OUT arrays) at the
    // `value` path so the encoder sees a structurally-matching value.
    let put_value = coerce_typed_put_value(&intro, value)?;
    encode_pv_field_with_bitset(&put_value, &intro, &changed, 0, order, &mut payload);
    let header = PvaHeader::application(false, order, Command::Put.code(), payload.len() as u32);
    let mut frame = Vec::new();
    header.write_into(&mut frame);
    frame.extend_from_slice(&payload);
    server.send_for_channel(sid, frame).await?;

    let done_frame = await_frame(&mut stream, op_timeout).await?;
    let result = match decode_op_or_reset(&server, &done_frame, Some(&intro))? {
        OpResponse::Status(s) => {
            if s.status.is_success() {
                Ok(())
            } else {
                Err(PvaError::Protocol(format!("PUT failed: {:?}", s.status)))
            }
        }
        other => {
            // Wrong response kind for the PUT completion step == impossible
            // op state → connection-fatal (pvxs clientget.cpp:456-493).
            server.close();
            Err(PvaError::Protocol(format!(
                "expected PUT done, got {other:?}"
            )))
        }
    };

    ioid_guard.disarm();
    let destroy = codec.build_destroy_request(sid, ioid);
    let _ = server.send_for_channel(sid, destroy).await;
    result
}

async fn op_put_inner(
    channel: &Arc<Channel>,
    value_str: &str,
    raw_pv_req: Option<&[u8]>,
    op_timeout: Duration,
) -> PvaResult<()> {
    let (server, sid) = ensure_active_with_op_timeout(channel, op_timeout).await?;
    let order = server.byte_order;
    let big_endian = matches!(order, ByteOrder::Big);
    let codec = PvaCodec { big_endian };
    let ioid = alloc_ioid();

    let pv_req = match raw_pv_req {
        Some(b) => b.to_vec(),
        None => build_pv_request_value_only(big_endian),
    };
    let mut stream = server.register_ioid_stream(sid, ioid, Command::Put.code());
    let mut ioid_guard = IoidGuard::new(server.clone(), ioid);
    let cache = server.type_cache();

    // INIT
    let init_req = codec.build_put_init(sid, ioid, &pv_req);
    server.send_for_channel(sid, init_req).await?;
    let init_frame = await_frame(&mut stream, op_timeout).await?;
    let init = match decode_op_cached_or_reset(&server, &init_frame, None, &mut cache.lock())? {
        OpResponse::Init(i) => i,
        other => {
            // Wrong response kind for this op step == impossible op state.
            // pvxs `M.fault()`s and `bev.reset()`s (clientget.cpp:456-493).
            server.close();
            server.unregister_ioid(ioid);
            return Err(PvaError::Protocol(format!(
                "expected PUT INIT, got {other:?}"
            )));
        }
    };
    if !init.status.is_success() {
        server.unregister_ioid(ioid);
        return Err(PvaError::Protocol(format!(
            "PUT INIT failed: {:?}",
            init.status
        )));
    }
    ioid_guard.arm_destroy(sid);
    let intro = init.introspection;

    // Build value matching introspection.
    let value = build_put_value(&intro, value_str)?;

    // DATA
    let mut payload = Vec::new();
    payload.put_u32(sid, order);
    payload.put_u32(ioid, order);
    payload.put_u8(0x00);
    let mut changed = BitSet::new();
    if let Some(bit) = intro.bit_for_path("value") {
        changed.set(bit);
    } else {
        changed.set(0);
    }
    changed.write_into(order, &mut payload);
    // pvxs `from_wire_valid` (serverget.cpp:451) decodes a BitSet delta —
    // only the fields whose bit is set. Encode consistently.
    encode_pv_field_with_bitset(&value, &intro, &changed, 0, order, &mut payload);
    let header = PvaHeader::application(false, order, Command::Put.code(), payload.len() as u32);
    let mut frame = Vec::new();
    header.write_into(&mut frame);
    frame.extend_from_slice(&payload);
    server.send_for_channel(sid, frame).await?;

    let done_frame = await_frame(&mut stream, op_timeout).await?;
    let result = match decode_op_or_reset(&server, &done_frame, Some(&intro))? {
        OpResponse::Status(s) => {
            if s.status.is_success() {
                Ok(())
            } else {
                Err(PvaError::Protocol(format!("PUT failed: {:?}", s.status)))
            }
        }
        other => {
            // Wrong response kind for the PUT completion step == impossible
            // op state → connection-fatal (pvxs clientget.cpp:456-493).
            server.close();
            Err(PvaError::Protocol(format!(
                "expected PUT done, got {other:?}"
            )))
        }
    };

    ioid_guard.disarm();
    let destroy = codec.build_destroy_request(sid, ioid);
    let _ = server.send_for_channel(sid, destroy).await;
    server.unregister_ioid(ioid);
    result
}

// ── MONITOR (with reconnect) ───────────────────────────────────────────

/// Typed monitor event delivered to callers of [`op_monitor_events`].
/// Mirrors pvxs's separation of `Connected` / `Disconnect` / `Finished`
/// / data exceptions thrown from `Subscription::pop()` (client.h:209).
#[derive(Debug, Clone)]
pub enum MonitorEvent {
    /// Channel just transitioned to Active and the server has
    /// confirmed our INIT/START. Fires once per connect cycle.
    Connected,
    /// Server pushed a value update.
    Data { intro: FieldDesc, value: PvField },
    /// Channel left Active (TCP closed, op error, channel closed).
    Disconnected,
    /// Server signalled end-of-stream via subcmd=0x10 (no further
    /// updates will arrive for this monitor).
    Finished,
}

/// Per-call configuration for [`op_monitor_events`] / handle variants.
/// Mirrors pvxs `MonitorBuilder::maskConnected/maskDisconnected`.
#[derive(Debug, Clone, Copy)]
pub struct MonitorEventMask {
    /// When true, suppress [`MonitorEvent::Connected`].
    pub mask_connected: bool,
    /// When true, suppress [`MonitorEvent::Disconnected`] and
    /// [`MonitorEvent::Finished`].
    pub mask_disconnected: bool,
}

impl Default for MonitorEventMask {
    fn default() -> Self {
        // pvxs defaults: maskConnected=true, maskDisconnected=false.
        Self {
            mask_connected: true,
            mask_disconnected: false,
        }
    }
}

/// Per-subscription metrics, mirroring pvxs `SubscriptionStat`
/// (client.h:165-178).
///
/// pvxs's queue fields describe a client-side monitor queue the consumer
/// pops from (`Subscription::pop()`). This client instead delivers every
/// update synchronously through the user callback inside the monitor loop,
/// so there is no pop()-able queue — the pvxs queue fields (`n_queue`,
/// `n_cli_squash`, `max_queue`) are therefore 0 *by construction*, not
/// merely unimplemented. The remaining counters are Rust-specific delivery
/// / ACK telemetry pvxs does not define and are named distinctly so they
/// are not mistaken for the pvxs queue surface — in particular
/// `max_events_per_ack` is the ACK-window high-water mark that the previous
/// `max_queue` field conflated with pvxs `maxQueue`.
#[derive(Debug, Clone, Default)]
pub struct SubscriptionStat {
    // ── pvxs `SubscriptionStat` surface ──
    /// pvxs `nQueue`: updates currently queued awaiting consumer pop().
    /// Always 0 — this client has no pop()-able queue (callback delivery).
    pub n_queue: u32,
    /// pvxs `nSrvSquash`: count of value updates where the server reported
    /// at least one update dropped/squashed (overrun bitset non-empty).
    /// Populated by the decoded monitor loop. The RAW monitor loop forwards
    /// bytes without decoding the trailing overrun bitset, so raw-handle
    /// stats leave this 0 (the raw stream still carries the overrun bits to
    /// the consumer; see `op_monitor_raw*`).
    pub n_srv_squash: u64,
    /// pvxs `nCliSquash`: updates dropped due to client queue overflow.
    /// Always 0 — there is no client queue to overflow.
    pub n_cli_squash: u64,
    /// pvxs `maxQueue`: max client queue depth observed. Always 0 (no
    /// client queue). For the ACK-window high-water mark see
    /// `max_events_per_ack`.
    pub max_queue: u32,
    /// pvxs `limitQueue`: the configured pipeline/queue limit
    /// (`pipeline_size`). Preserved across `stats(reset)`.
    pub limit_queue: u32,

    // ── Rust-specific delivery / ACK telemetry (not in pvxs) ──
    /// Total updates delivered to the user callback.
    pub n_delivered: u64,
    /// Number of MONITOR_ACK frames sent (pipelined window cycles).
    pub n_acks: u64,
    /// Highest `events_since_ack` value the loop observed between ACKs.
    /// This is ACK-window telemetry, NOT pvxs `maxQueue`; with a healthy
    /// `pipeline_size` it stays close to `pipeline_size`.
    pub max_events_per_ack: u32,
}

/// Internal shared state — the monitor loop publishes to this on every
/// reconnect / event / pause toggle, and [`SubscriptionHandle`] reads
/// from it.
struct SubscriptionState {
    /// Active `(ServerConn, sid, ioid)` triple. Refreshed on every
    /// reconnect cycle. None when in the gap between connections.
    active: parking_lot::Mutex<
        Option<(
            Arc<super::server_conn::ServerConn>,
            u32, /*sid*/
            u32, /*ioid*/
        )>,
    >,
    paused: std::sync::atomic::AtomicBool,
    stop: std::sync::atomic::AtomicBool,
    stats: parking_lot::Mutex<SubscriptionStat>,
    /// Wakes a monitor loop that is parked in `stream.recv().await` so a
    /// cancel issued while no server data is arriving takes effect
    /// immediately instead of waiting for the next (possibly never)
    /// frame. `Notify` stores a single permit, so a cancel that races
    /// ahead of the loop reaching its `select!` is not lost.
    cancel: tokio::sync::Notify,
}

impl SubscriptionState {
    /// Single teardown owner for an active subscription. Mirrors pvxs
    /// `SubscriptionImpl::_cancel` (clientmon.cpp:295-317): it sets the
    /// terminal stop flag, atomically takes the live `(server, sid,
    /// ioid)` triple, sends a best-effort `DESTROY_REQUEST` so the
    /// server releases the IOID rather than waiting for the TCP circuit
    /// to die, unregisters the IOID, and finally wakes the monitor loop.
    ///
    /// `try_send` (not an awaiting send) is used so this is callable from
    /// both `Drop` (no async context) and the async cancel paths through
    /// the same owner. The destroy frame is enqueued to the writer task
    /// before any caller blocks on the loop — pvxs likewise performs the
    /// cancel operation before `syncCancel(true)` waits.
    fn teardown(&self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        let snapshot = self.active.lock().take();
        if let Some((server, sid, ioid)) = snapshot {
            let codec = PvaCodec {
                big_endian: matches!(server.byte_order, ByteOrder::Big),
            };
            let _ = server.try_send(codec.build_destroy_request(sid, ioid));
            server.unregister_ioid(ioid);
        }
        // Wake any loop parked in `stream.recv().await`. Even if the loop
        // has not reached its `select!` yet, `Notify` holds the permit so
        // the next `notified()` completes at once.
        self.cancel.notify_one();
    }
}

/// User-facing handle returned by [`op_monitor_handle`]. Drops cleanly
/// without aborting the inner task — call [`Self::stop`] explicitly to
/// signal teardown. Mirrors pvxs `Subscription` at the public-method
/// level.
pub struct SubscriptionHandle {
    state: Arc<SubscriptionState>,
    task: Option<tokio::task::JoinHandle<PvaResult<()>>>,
}

impl SubscriptionHandle {
    /// Pause server emissions on this subscription. Safe to call
    /// multiple times; second call is a no-op when already paused.
    /// Mirrors pvxs `Subscription::pause(true)` (clientmon.cpp:115).
    /// Best-effort — if the underlying connection is gone we set the
    /// flag and the loop applies it on next reconnect.
    pub async fn pause(&self) {
        let was_paused = self
            .state
            .paused
            .swap(true, std::sync::atomic::Ordering::Relaxed);
        if was_paused {
            return;
        }
        let snapshot = self.state.active.lock().clone();
        if let Some((server, sid, ioid)) = snapshot {
            let big_endian = matches!(server.byte_order, ByteOrder::Big);
            let codec = PvaCodec { big_endian };
            let _ = server
                .send_for_channel(sid, codec.build_monitor_pause(sid, ioid))
                .await;
        }
    }

    /// Resume a paused subscription. Mirrors pvxs
    /// `Subscription::pause(false)`.
    pub async fn resume(&self) {
        let was_paused = self
            .state
            .paused
            .swap(false, std::sync::atomic::Ordering::Relaxed);
        if !was_paused {
            return;
        }
        let snapshot = self.state.active.lock().clone();
        if let Some((server, sid, ioid)) = snapshot {
            let big_endian = matches!(server.byte_order, ByteOrder::Big);
            let codec = PvaCodec { big_endian };
            let _ = server
                .send_for_channel(sid, codec.build_monitor_resume(sid, ioid))
                .await;
        }
    }

    /// Snapshot the per-subscription metrics. pvxs `Subscription::stats`
    /// equivalent. The optional `reset` flag (pvxs 1.1.0+) zeros
    /// counters after read.
    pub fn stats(&self, reset: bool) -> SubscriptionStat {
        let mut lock = self.state.stats.lock();
        let snap = lock.clone();
        if reset {
            *lock = SubscriptionStat {
                limit_queue: lock.limit_queue,
                ..Default::default()
            };
        }
        snap
    }

    /// Signal the inner task to terminate (async — pvxs
    /// `syncCancel(false)` analog). Routes through the single teardown
    /// owner so the server-side IOID is released and a loop parked in
    /// `stream.recv().await` is woken immediately; does not await the
    /// task. Drop alone does not stop the task — call this explicitly.
    pub fn stop(&self) {
        self.state.teardown();
    }

    /// Stop and await termination. pvxs `syncCancel(true)` analog —
    /// once this returns no further callbacks will fire.
    ///
    /// The teardown (DESTROY_REQUEST + IOID unregister + loop wake) runs
    /// **before** awaiting the task, matching pvxs `_cancel`, which
    /// performs the cancel operation before `syncCancel(true)` blocks
    /// (clientmon.cpp:295-317, 810-824). Without this the task could be
    /// parked forever in `stream.recv().await` on an idle monitor and
    /// the await would never return.
    pub async fn stop_sync(mut self) {
        self.state.teardown();
        if let Some(t) = self.task.take() {
            let _ = t.await;
        }
    }

    /// Await the inner task without signalling stop. Returns whatever
    /// the loop returned (Ok on clean channel close, Err on fatal).
    /// Used by long-lived consumers (the bridge gateway) that want to
    /// observe the natural lifetime of the subscription while still
    /// holding a [`Pauser`] cloned out beforehand.
    pub async fn wait(mut self) -> PvaResult<()> {
        if let Some(t) = self.task.take() {
            match t.await {
                Ok(r) => r,
                Err(_) => Ok(()),
            }
        } else {
            Ok(())
        }
    }

    /// True if the inner task has finished (channel closed, fatal
    /// error, or `stop()` was called and the loop drained). Use to
    /// drive an auto-restart wrapper without consuming the handle.
    pub fn is_done(&self) -> bool {
        self.task.as_ref().map(|t| t.is_finished()).unwrap_or(true)
    }

    /// A cheap clone-able handle that can pause/resume the
    /// subscription from an unrelated task (no ownership of the
    /// underlying JoinHandle). Used by the PVA gateway to forward
    /// downstream watermark events into upstream pipeline-pause
    /// control messages — pvxs `MonitorControlOp::pipeline` parity.
    pub fn pauser(&self) -> Pauser {
        Pauser {
            state: self.state.clone(),
        }
    }
}

/// Drop-time cleanup: when a SubscriptionHandle is dropped without an
/// explicit `stop()` / `stop_sync()`, signal the inner loop to bail and
/// fire a best-effort DESTROY_REQUEST so the server releases the IOID
/// slot rather than waiting for the TCP circuit to die. The send is
/// non-blocking (`try_send`) because Drop runs on whichever runtime the
/// handle was dropped on, and we'd rather drop the cleanup frame than
/// stall a runtime worker.
impl Drop for SubscriptionHandle {
    fn drop(&mut self) {
        // Route through the single teardown owner: sets stop, takes the
        // live (server, sid, ioid) triple, sends a best-effort
        // DESTROY_REQUEST, unregisters the IOID, and wakes the loop.
        // Reconnect-gap drops have nothing to send (active is None),
        // which is fine — the next reconnect cycle won't fire because
        // `stop` is now set. Idempotent, so a drop after `stop_sync`
        // (which already tore down) is a no-op.
        self.state.teardown();
        // Don't await/abort the task here — letting it run to a clean
        // exit on the woken `select!` matches the existing
        // `stop()`-then-drop semantics. Callers that need synchronous
        // teardown should call `stop_sync().await`.
    }
}

/// Detached pause/resume handle — see [`SubscriptionHandle::pauser`].
#[derive(Clone)]
pub struct Pauser {
    state: Arc<SubscriptionState>,
}

impl Pauser {
    /// Same semantics as [`SubscriptionHandle::pause`]. Async because
    /// it sends a control message to the server.
    pub async fn pause(&self) {
        let was_paused = self
            .state
            .paused
            .swap(true, std::sync::atomic::Ordering::Relaxed);
        if was_paused {
            return;
        }
        let snapshot = self.state.active.lock().clone();
        if let Some((server, sid, ioid)) = snapshot {
            let big_endian = matches!(server.byte_order, ByteOrder::Big);
            let codec = PvaCodec { big_endian };
            let _ = server
                .send_for_channel(sid, codec.build_monitor_pause(sid, ioid))
                .await;
        }
    }

    /// Same semantics as [`SubscriptionHandle::resume`].
    pub async fn resume(&self) {
        let was_paused = self
            .state
            .paused
            .swap(false, std::sync::atomic::Ordering::Relaxed);
        if !was_paused {
            return;
        }
        let snapshot = self.state.active.lock().clone();
        if let Some((server, sid, ioid)) = snapshot {
            let big_endian = matches!(server.byte_order, ByteOrder::Big);
            let codec = PvaCodec { big_endian };
            let _ = server
                .send_for_channel(sid, codec.build_monitor_resume(sid, ioid))
                .await;
        }
    }
}

/// classification of a frame arriving on the raw MONITOR
/// stream. The control-frame policy lives here as one pure, testable
/// decision so a malformed control frame cannot be silently skipped or
/// reported as a clean end-of-stream — the two swallow bugs the raw
/// loop previously had (`payload.len() < 5 => continue` and a FINISH
/// `Status::decode` failure falling through to `Ok(())`).
#[derive(Debug)]
enum RawMonitorFrameKind {
    /// subcmd `0x00` — a DATA frame; the caller forwards `payload[5..]`.
    Data,
    /// A non-DATA, non-FINISH control frame (e.g. a pipeline ACK echo) —
    /// the caller ignores it.
    Skip,
    /// FINISH (`subcmd & 0x10`) carrying a success Status — clean end of
    /// stream.
    FinishOk,
    /// A fatal condition: a truncated frame (shorter than `ioid +
    /// subcmd`), a FINISH whose required Status cannot be decoded, or a
    /// FINISH carrying a non-success Status. The caller surfaces
    /// `MonitorEnd::Fatal`.
    Fatal(PvaError),
}

/// classify a raw MONITOR stream frame. Mirrors the typed path's
/// `Status::decode` owner — a missing/malformed FINISH Status is an
/// error, not a clean end. pvxs resets the connection when a monitor
/// message decode is not good (`clientmon.cpp:596`).
fn classify_raw_monitor_frame(payload: &[u8], order: ByteOrder) -> RawMonitorFrameKind {
    // A MONITOR application frame always carries ioid (4) + subcmd (1).
    // A shorter payload is a truncated control frame, not one to skip.
    if payload.len() < 5 {
        return RawMonitorFrameKind::Fatal(PvaError::Decode(format!(
            "MONITOR frame too short: {} bytes (need >= 5 for ioid+subcmd)",
            payload.len()
        )));
    }
    let subcmd = payload[4];
    if subcmd == 0x00 {
        return RawMonitorFrameKind::Data;
    }
    if subcmd & 0x10 != 0 {
        // FINISH carries a required Status after the subcmd. A decode
        // failure must NOT degrade to a clean end-of-stream — that would
        // hide an upstream protocol error from a forwarding gateway.
        let mut cur = std::io::Cursor::new(&payload[5..]);
        return match crate::proto::Status::decode(&mut cur, order) {
            Ok(st) if st.is_success() => RawMonitorFrameKind::FinishOk,
            Ok(st) => RawMonitorFrameKind::Fatal(PvaError::Protocol(format!(
                "MONITOR FINISH with non-success status: {st:?}"
            ))),
            Err(e) => RawMonitorFrameKind::Fatal(PvaError::Decode(format!(
                "MONITOR FINISH status decode failed: {e}"
            ))),
        };
    }
    // Non-DATA, non-FINISH control frame (pipeline ACK echo etc.).
    RawMonitorFrameKind::Skip
}

/// Raw-frame monitor entry: like [`op_monitor`] but the
/// callback receives the **raw MONITOR DATA body bytes** (the
/// `changed | value | overrun` triplet from the wire) instead of a
/// decoded [`PvField`]. Bridge gateways feed these directly into
/// [`crate::server_native::RawMonitorEvent`] for downstream
/// re-emission without an intermediate decode.
///
/// Callback shape: `(intro: &FieldDesc, body: bytes::Bytes,
/// byte_order: ByteOrder)`. Body is refcount-shared (cheap clone).
pub async fn op_monitor_raw_frames<F>(
    channel: &Arc<Channel>,
    fields: &[&str],
    pipeline_size: u32,
    mut callback: F,
) -> PvaResult<()>
where
    F: FnMut(&FieldDesc, bytes::Bytes, ByteOrder) + Send,
{
    let fields_owned: Vec<String> = fields.iter().map(|s| s.to_string()).collect();
    loop {
        let (server, sid) = match channel.ensure_active().await {
            Ok(p) => p,
            Err(e) => {
                if matches!(
                    channel.current_state(),
                    super::channel::ChannelState::Closed
                ) {
                    return Ok(());
                }
                debug!(pv = %channel.pv_name, err = %e,
                    "raw monitor reconnect failed; retrying in 500ms");
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                continue;
            }
        };
        match run_raw_monitor_loop(
            server.clone(),
            sid,
            &fields_owned,
            pipeline_size,
            &mut callback,
            None,
        )
        .await
        {
            Ok(()) => return Ok(()),
            Err(MonitorEnd::ChannelClosed) => return Ok(()),
            Err(MonitorEnd::ConnectionLost) => {
                debug!(pv = %channel.pv_name, "raw monitor lost connection; will retry");
                if matches!(
                    channel.current_state(),
                    super::channel::ChannelState::Closed
                ) {
                    return Ok(());
                }
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
            Err(MonitorEnd::Fatal(e)) => return Err(e),
        }
    }
}

/// Like [`op_monitor_raw_frames`] but returns a
/// [`SubscriptionHandle`] for pause/resume/stats. The inner raw
/// monitor loop runs in a spawned task so the bridge gateway can wire
/// downstream watermark events into upstream pipeline-pause control
/// messages without an intermediate decode/encode pass.
pub fn op_monitor_raw_frames_handle<F>(
    channel: Arc<Channel>,
    fields: &[&str],
    pipeline_size: u32,
    mut callback: F,
) -> SubscriptionHandle
where
    F: FnMut(&FieldDesc, bytes::Bytes, ByteOrder) + Send + 'static,
{
    let fields_owned: Vec<String> = fields.iter().map(|s| s.to_string()).collect();
    let state = Arc::new(SubscriptionState {
        active: parking_lot::Mutex::new(None),
        paused: std::sync::atomic::AtomicBool::new(false),
        stop: std::sync::atomic::AtomicBool::new(false),
        stats: parking_lot::Mutex::new(SubscriptionStat {
            limit_queue: pipeline_size,
            ..Default::default()
        }),
        cancel: tokio::sync::Notify::new(),
    });
    let state_for_task = state.clone();

    let task = tokio::spawn(async move {
        loop {
            if state_for_task
                .stop
                .load(std::sync::atomic::Ordering::Relaxed)
            {
                return Ok(());
            }
            let (server, sid) = match channel.ensure_active().await {
                Ok(p) => p,
                Err(e) => {
                    if matches!(
                        channel.current_state(),
                        super::channel::ChannelState::Closed
                    ) {
                        return Ok(());
                    }
                    debug!(pv = %channel.pv_name, err = %e,
                        "raw monitor reconnect failed; retrying in 500ms");
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    continue;
                }
            };
            match run_raw_monitor_loop(
                server.clone(),
                sid,
                &fields_owned,
                pipeline_size,
                &mut callback,
                Some(state_for_task.clone()),
            )
            .await
            {
                Ok(()) => return Ok(()),
                Err(MonitorEnd::ChannelClosed) => return Ok(()),
                Err(MonitorEnd::ConnectionLost) => {
                    state_for_task.active.lock().take();
                    if matches!(
                        channel.current_state(),
                        super::channel::ChannelState::Closed
                    ) {
                        return Ok(());
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                }
                Err(MonitorEnd::Fatal(e)) => return Err(e),
            }
        }
    });

    SubscriptionHandle {
        state,
        task: Some(task),
    }
}

async fn run_raw_monitor_loop<F>(
    server: Arc<super::server_conn::ServerConn>,
    sid: u32,
    fields: &[String],
    pipeline_size: u32,
    callback: &mut F,
    state: Option<Arc<SubscriptionState>>,
) -> Result<(), MonitorEnd>
where
    F: FnMut(&FieldDesc, bytes::Bytes, ByteOrder) + Send,
{
    let order = server.byte_order;
    let big_endian = matches!(order, ByteOrder::Big);
    let codec = PvaCodec { big_endian };
    let ioid = alloc_ioid();
    // when `pipeline_size > 0`, inject
    // `record._options.pipeline = "true"` + `queueSize` into the
    // pvRequest and set the MONITOR INIT pipeline bit + initial
    // nack trailer. Server-side credit window is keyed on the
    // pvRequest options (pvxs servermon.cpp:523-552); pre-fix Rust
    // sent the pipeline size on START as a trailer the server never
    // read.
    let refs: Vec<&str> = fields.iter().map(|s| s.as_str()).collect();
    let pv_req: std::borrow::Cow<'_, [u8]> = if pipeline_size > 0 {
        // Empty field list → empty `field {}` sub-structure, which
        // `request_to_mask` reads as "select the whole structure"
        // (pv_request.rs `request_field.is_empty()`). Forcing a
        // `field(value)` here narrowed the default monitor to the
        // `value` leaf and broke any PV whose top-level descriptor is
        // not a structure with a `value` member (e.g. a bare-scalar
        // `SharedPV`): the server rejects `field(value)` with
        // `RequestMaskError::EmptyMask`.
        std::borrow::Cow::Owned(crate::pv_request::build_pv_request_pipeline(
            &refs,
            pipeline_size,
            big_endian,
        ))
    } else if fields.is_empty() {
        std::borrow::Cow::Borrowed(sentinel_all_fields())
    } else {
        std::borrow::Cow::Owned(build_pv_request_fields(&refs, big_endian))
    };
    let mut stream = server.register_ioid_stream(sid, ioid, Command::Monitor.code());
    let init_req = codec.build_monitor_init(
        sid,
        ioid,
        &pv_req,
        (pipeline_size > 0).then_some(pipeline_size),
    );
    server
        .send_for_channel(sid, init_req)
        .await
        .map_err(|_| MonitorEnd::ConnectionLost)?;
    let init_frame = stream.recv().await.ok_or(MonitorEnd::ConnectionLost)?;
    let cache = server.type_cache();
    let init = match decode_op_response_cached(&init_frame, None, &mut cache.lock()) {
        Ok(OpResponse::Init(i)) => i,
        Ok(other) => {
            server.unregister_ioid(ioid);
            return Err(MonitorEnd::Fatal(PvaError::Protocol(format!(
                "expected MONITOR INIT, got {other:?}"
            ))));
        }
        Err(e) => {
            server.unregister_ioid(ioid);
            return Err(MonitorEnd::Fatal(e));
        }
    };
    if !init.status.is_success() {
        server.unregister_ioid(ioid);
        return Err(MonitorEnd::Fatal(PvaError::Protocol(format!(
            "MONITOR INIT failed: {:?}",
            init.status
        ))));
    }
    let intro = init.introspection;
    // raw-path Pauser support: honour the handle's prior
    // pause state so a SubscriptionHandle::pause() called before
    // reconnect stays paused after the resubscribe. Mirrors the
    // typed `run_monitor_loop` path.
    let initially_paused = state
        .as_ref()
        .map(|s| s.paused.load(std::sync::atomic::Ordering::Relaxed))
        .unwrap_or(false);
    let start = if initially_paused {
        codec.build_monitor_pause(sid, ioid)
    } else {
        codec.build_monitor_start(sid, ioid)
    };
    server
        .send_for_channel(sid, start)
        .await
        .map_err(|_| MonitorEnd::ConnectionLost)?;
    if let Some(s) = &state {
        *s.active.lock() = Some((server.clone(), sid, ioid));
    }
    let mut events_since_ack: u32 = 0;
    loop {
        // Honour stop() — caller dropped the handle or called
        // stop_sync().
        let frame = match &state {
            // Handle present: race the next frame against an explicit
            // cancel so a stop issued while no server data is arriving
            // wakes the loop immediately instead of parking forever on an
            // idle monitor. pvxs wakes the worker the same way — `_cancel`
            // runs through `loop.tryInvoke` (clientmon.cpp:810-824).
            Some(s) => {
                if s.stop.load(std::sync::atomic::Ordering::Relaxed) {
                    server.unregister_ioid(ioid);
                    s.active.lock().take();
                    return Err(MonitorEnd::ChannelClosed);
                }
                tokio::select! {
                    biased;
                    _ = s.cancel.notified() => {
                        // The teardown owner already took `active` and
                        // unregistered the IOID; both calls are
                        // idempotent, repeated here so the no-handle path
                        // and this path converge on the same cleanup.
                        server.unregister_ioid(ioid);
                        s.active.lock().take();
                        return Err(MonitorEnd::ChannelClosed);
                    }
                    f = stream.recv() => match f {
                        Some(f) => f,
                        None => {
                            server.unregister_ioid(ioid);
                            s.active.lock().take();
                            return Err(MonitorEnd::ConnectionLost);
                        }
                    },
                }
            }
            // No handle: nothing can cancel this loop, so just await the
            // next frame.
            None => match stream.recv().await {
                Some(f) => f,
                None => {
                    server.unregister_ioid(ioid);
                    return Err(MonitorEnd::ConnectionLost);
                }
            },
        };
        // classify the frame through the single control-frame
        // owner. A too-short frame and a FINISH with a missing/malformed
        // Status are fatal — never silently skipped (`continue`) nor
        // degraded to a clean end (`Ok(())`), which would hide upstream
        // protocol corruption from a forwarding gateway.
        match classify_raw_monitor_frame(&frame.payload, order) {
            // subcmd 0x00: DATA — fall through to body forwarding below.
            RawMonitorFrameKind::Data => {}
            // Non-DATA, non-FINISH control frame (pipeline ACK echo); we
            // drive ACKs ourselves, so ignore it.
            RawMonitorFrameKind::Skip => continue,
            RawMonitorFrameKind::FinishOk => {
                server.unregister_ioid(ioid);
                // clear the handle's `active` tuple on FINISH so
                // a later `pause()` / `resume()` / `drop()` doesn't act on
                // a (sid, ioid) the client has already unregistered and the
                // server has already finalised. pvxs `clientmon.cpp:720-729`
                // treats FINISH as the operation-owner cleanup path:
                // state→Done, IOID maps erased, no DESTROY sent.
                if let Some(s) = &state {
                    s.active.lock().take();
                }
                return Ok(());
            }
            RawMonitorFrameKind::Fatal(e) => {
                server.unregister_ioid(ioid);
                if let Some(s) = &state {
                    s.active.lock().take();
                }
                return Err(MonitorEnd::Fatal(e));
            }
        }
        // Body = payload[5..] = changed | value | overrun (raw).
        // Wrap in `Bytes` so the broadcast fan-out shares this
        // allocation refcount-style.
        let body = bytes::Bytes::copy_from_slice(&frame.payload[5..]);
        callback(&intro, body, order);
        events_since_ack += 1;
        if let Some(s) = &state {
            let mut st = s.stats.lock();
            st.n_delivered += 1;
            // this raw forwarding path's contract is to relay
            // the body (changed | value | overrun) downstream WITHOUT an
            // intermediate decode, so it does not parse the trailing
            // overrun bitset and `n_srv_squash` is not derived here.
            // The overrun travels intact in `body` and is decoded by the
            // downstream consumer; the typed `run_monitor_loop` path
            // populates `n_srv_squash`. Adding a full value decode purely
            // to read the overrun would defeat this path's purpose.
            if events_since_ack > st.max_events_per_ack {
                st.max_events_per_ack = events_since_ack;
            }
        }
        if pipeline_size > 0 && events_since_ack >= ack_threshold(pipeline_size) {
            let ack = codec.build_monitor_ack(sid, ioid, events_since_ack);
            if server.send_for_channel(sid, ack).await.is_err() {
                server.unregister_ioid(ioid);
                return Err(MonitorEnd::ConnectionLost);
            }
            if let Some(s) = &state {
                s.stats.lock().n_acks += 1;
            }
            events_since_ack = 0;
        }
    }
}

pub async fn op_monitor<F>(
    channel: &Arc<Channel>,
    fields: &[&str],
    pipeline_size: u32,
    callback: F,
) -> PvaResult<()>
where
    F: FnMut(&FieldDesc, &PvField) + Send,
{
    let fields_owned: Vec<String> = fields.iter().map(|s| s.to_string()).collect();
    op_monitor_inner(
        channel,
        fields_owned,
        None,
        MonitorFlow::window(pipeline_size),
        callback,
    )
    .await
}

/// `op_monitor` variant accepting a pre-built pvRequest blob. Threads
/// `record[queueSize=N,pipeline=true,...]` and custom field-mask
/// selections through to MONITOR INIT. pvxs
/// `Context::monitor(name).pvRequest(...)` parity. The raw bytes win
/// over the field-list path; field reconnect-replay still works
/// because the bytes are reused on every reconnect cycle.
pub async fn op_monitor_raw<F>(
    channel: &Arc<Channel>,
    pv_req: Vec<u8>,
    flow: MonitorFlow,
    callback: F,
) -> PvaResult<()>
where
    F: FnMut(&FieldDesc, &PvField) + Send,
{
    op_monitor_inner(channel, Vec::new(), Some(pv_req), flow, callback).await
}

async fn op_monitor_inner<F>(
    channel: &Arc<Channel>,
    fields_owned: Vec<String>,
    raw_pv_req: Option<Vec<u8>>,
    flow: MonitorFlow,
    mut callback: F,
) -> PvaResult<()>
where
    F: FnMut(&FieldDesc, &PvField) + Send,
{
    loop {
        let (server, sid) = match channel.ensure_active().await {
            Ok(p) => p,
            Err(e) => {
                if matches!(
                    channel.current_state(),
                    super::channel::ChannelState::Closed
                ) {
                    return Ok(());
                }
                debug!(pv = %channel.pv_name, err = %e, "monitor reconnect failed; retrying in 500ms");
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                continue;
            }
        };
        match run_monitor_loop(
            server.clone(),
            sid,
            &fields_owned,
            raw_pv_req.as_deref(),
            flow,
            &mut callback,
            None,
        )
        .await
        {
            Ok(()) => return Ok(()),
            Err(MonitorEnd::ChannelClosed) => return Ok(()),
            Err(MonitorEnd::ConnectionLost) => {
                debug!(pv = %channel.pv_name, "monitor lost connection; will retry");
                if matches!(
                    channel.current_state(),
                    super::channel::ChannelState::Closed
                ) {
                    return Ok(());
                }
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
            Err(MonitorEnd::Fatal(e)) => return Err(e),
        }
    }
}

/// Like [`op_monitor`] but returns a [`SubscriptionHandle`] for
/// pause/resume/stats. The inner monitor loop runs in a spawned task
/// and stops when the handle's `stop()` is called or when the channel
/// is closed.
pub fn op_monitor_handle<F>(
    channel: Arc<Channel>,
    fields: &[&str],
    pipeline_size: u32,
    mut callback: F,
) -> SubscriptionHandle
where
    F: FnMut(&FieldDesc, &PvField) + Send + 'static,
{
    let fields_owned: Vec<String> = fields.iter().map(|s| s.to_string()).collect();
    let flow = MonitorFlow::window(pipeline_size);
    let state = Arc::new(SubscriptionState {
        active: parking_lot::Mutex::new(None),
        paused: std::sync::atomic::AtomicBool::new(false),
        stop: std::sync::atomic::AtomicBool::new(false),
        stats: parking_lot::Mutex::new(SubscriptionStat {
            limit_queue: flow.queue_size,
            ..Default::default()
        }),
        cancel: tokio::sync::Notify::new(),
    });
    let state_for_task = state.clone();

    let task = tokio::spawn(async move {
        loop {
            if state_for_task
                .stop
                .load(std::sync::atomic::Ordering::Relaxed)
            {
                return Ok(());
            }
            let (server, sid) = match channel.ensure_active().await {
                Ok(p) => p,
                Err(e) => {
                    if matches!(
                        channel.current_state(),
                        super::channel::ChannelState::Closed
                    ) {
                        return Ok(());
                    }
                    debug!(pv = %channel.pv_name, err = %e, "monitor reconnect failed; retrying in 500ms");
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    continue;
                }
            };
            match run_monitor_loop(
                server.clone(),
                sid,
                &fields_owned,
                None,
                flow,
                &mut callback,
                Some(state_for_task.clone()),
            )
            .await
            {
                Ok(()) => return Ok(()),
                Err(MonitorEnd::ChannelClosed) => return Ok(()),
                Err(MonitorEnd::ConnectionLost) => {
                    state_for_task.active.lock().take();
                    if matches!(
                        channel.current_state(),
                        super::channel::ChannelState::Closed
                    ) {
                        return Ok(());
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                }
                Err(MonitorEnd::Fatal(e)) => return Err(e),
            }
        }
    });

    SubscriptionHandle {
        state,
        task: Some(task),
    }
}

/// Run a monitor and deliver [`MonitorEvent`] values to `callback`.
/// Bridges the per-update `(FieldDesc, PvField)` shape of the inner
/// loop to pvxs's typed event stream. The mask flags control whether
/// `Connected`/`Disconnected`/`Finished` events surface or stay
/// suppressed (pvxs `maskConnected` / `maskDisconnected`).
pub async fn op_monitor_events<F>(
    channel: &Arc<Channel>,
    fields: &[&str],
    pipeline_size: u32,
    mask: MonitorEventMask,
    mut callback: F,
) -> PvaResult<()>
where
    F: FnMut(MonitorEvent) + Send,
{
    let fields_owned: Vec<String> = fields.iter().map(|s| s.to_string()).collect();
    let flow = MonitorFlow::window(pipeline_size);
    loop {
        let (server, sid) = match channel.ensure_active().await {
            Ok(p) => p,
            Err(e) => {
                if matches!(
                    channel.current_state(),
                    super::channel::ChannelState::Closed
                ) {
                    return Ok(());
                }
                debug!(pv = %channel.pv_name, err = %e, "monitor reconnect failed; retrying in 500ms");
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                continue;
            }
        };
        if !mask.mask_connected {
            callback(MonitorEvent::Connected);
        }
        let mut data_callback = |intro: &FieldDesc, value: &PvField| {
            callback(MonitorEvent::Data {
                intro: intro.clone(),
                value: value.clone(),
            });
        };
        let result = run_monitor_loop(
            server.clone(),
            sid,
            &fields_owned,
            None,
            flow,
            &mut data_callback,
            None,
        )
        .await;
        match result {
            Ok(()) => {
                if !mask.mask_disconnected {
                    callback(MonitorEvent::Finished);
                }
                return Ok(());
            }
            Err(MonitorEnd::ChannelClosed) => {
                if !mask.mask_disconnected {
                    callback(MonitorEvent::Disconnected);
                }
                return Ok(());
            }
            Err(MonitorEnd::ConnectionLost) => {
                if !mask.mask_disconnected {
                    callback(MonitorEvent::Disconnected);
                }
                if matches!(
                    channel.current_state(),
                    super::channel::ChannelState::Closed
                ) {
                    return Ok(());
                }
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
            Err(MonitorEnd::Fatal(e)) => return Err(e),
        }
    }
}

#[derive(Debug)]
#[allow(dead_code)]
enum MonitorEnd {
    ChannelClosed,
    ConnectionLost,
    Fatal(PvaError),
}

async fn run_monitor_loop<F>(
    server: Arc<super::server_conn::ServerConn>,
    sid: u32,
    fields: &[String],
    raw_pv_req: Option<&[u8]>,
    flow: MonitorFlow,
    callback: &mut F,
    state: Option<Arc<SubscriptionState>>,
) -> Result<(), MonitorEnd>
where
    F: FnMut(&FieldDesc, &PvField) + Send,
{
    let order = server.byte_order;
    let big_endian = matches!(order, ByteOrder::Big);
    let codec = PvaCodec { big_endian };
    let ioid = alloc_ioid();

    // A caller-supplied `raw_pv_req` is sent verbatim — `flow` was
    // derived from that same request's `record._options`, so the wire
    // pipeline/queueSize and the INIT `nack` trailer below share one
    // origin. The auto-built pvRequest (no caller request) injects the
    // pipeline options only when `flow.pipeline`, i.e. the client's
    // configured window is on.
    let pv_req: std::borrow::Cow<'_, [u8]> = match raw_pv_req {
        Some(b) => std::borrow::Cow::Borrowed(b),
        None if flow.pipeline => {
            // Empty field list → empty `field {}` (= whole structure);
            // see `op_monitor_raw_frames`. Forcing `field(value)` broke
            // bare-scalar PVs with a server-side `EmptyMask` reject.
            let refs: Vec<&str> = fields.iter().map(|s| s.as_str()).collect();
            std::borrow::Cow::Owned(crate::pv_request::build_pv_request_pipeline(
                &refs,
                flow.queue_size,
                big_endian,
            ))
        }
        None if fields.is_empty() => std::borrow::Cow::Borrowed(sentinel_all_fields()),
        None => {
            let refs: Vec<&str> = fields.iter().map(|s| s.as_str()).collect();
            std::borrow::Cow::Owned(build_pv_request_fields(&refs, big_endian))
        }
    };

    let mut stream = server.register_ioid_stream(sid, ioid, Command::Monitor.code());

    // INIT — the pipeline bit + initial `nack` (credit window) trailer
    // are written iff `flow.pipeline`, carrying the negotiated
    // `queue_size` (pvxs `clientmon.cpp:333-348` writes `queueSize` only
    // `if(pipeline)`).
    let init_req =
        codec.build_monitor_init(sid, ioid, &pv_req, flow.pipeline.then_some(flow.queue_size));
    server
        .send_for_channel(sid, init_req)
        .await
        .map_err(|_| MonitorEnd::ConnectionLost)?;
    let init_frame = stream.recv().await.ok_or(MonitorEnd::ConnectionLost)?;
    let cache = server.type_cache();
    let init = match decode_op_response_cached(&init_frame, None, &mut cache.lock()) {
        Ok(OpResponse::Init(i)) => i,
        Ok(other) => {
            server.unregister_ioid(ioid);
            return Err(MonitorEnd::Fatal(PvaError::Protocol(format!(
                "expected MONITOR INIT, got {other:?}"
            ))));
        }
        Err(e) => {
            server.unregister_ioid(ioid);
            return Err(MonitorEnd::Fatal(e));
        }
    };
    if !init.status.is_success() {
        server.unregister_ioid(ioid);
        return Err(MonitorEnd::Fatal(PvaError::Protocol(format!(
            "MONITOR INIT failed: {:?}",
            init.status
        ))));
    }
    let intro = init.introspection;

    // START with pipeline ack window — unless the handle was paused
    // before this reconnect cycle, in which case start in STOP state
    // so the server doesn't begin emitting until resume() is called.
    let initially_paused = state
        .as_ref()
        .map(|s| s.paused.load(std::sync::atomic::Ordering::Relaxed))
        .unwrap_or(false);
    let start = if initially_paused {
        codec.build_monitor_pause(sid, ioid)
    } else {
        codec.build_monitor_start(sid, ioid)
    };
    server
        .send_for_channel(sid, start)
        .await
        .map_err(|_| MonitorEnd::ConnectionLost)?;

    if let Some(s) = &state {
        *s.active.lock() = Some((server.clone(), sid, ioid));
    }

    let mut events_since_ack: u32 = 0;
    // pvxs cc5d382: track the most-recently-delivered "complete"
    // value so partial updates (bitset with only some fields marked)
    // can be merged with prior state before the user callback runs.
    // Without this, unmarked leaves would land at the consumer as
    // zero-filled defaults — sparse delta semantics — which loses
    // the cumulative state pvxs guarantees.
    let mut prior: Option<PvField> = None;
    loop {
        let frame = match &state {
            // Handle present: race the next frame against an explicit
            // cancel so a stop issued while no server data is arriving
            // wakes the loop immediately instead of parking forever on an
            // idle monitor. pvxs wakes the worker the same way — `_cancel`
            // runs through `loop.tryInvoke` (clientmon.cpp:810-824).
            Some(s) => {
                if s.stop.load(std::sync::atomic::Ordering::Relaxed) {
                    server.unregister_ioid(ioid);
                    s.active.lock().take();
                    return Err(MonitorEnd::ChannelClosed);
                }
                tokio::select! {
                    biased;
                    _ = s.cancel.notified() => {
                        // The teardown owner already took `active` and
                        // unregistered the IOID; both calls are
                        // idempotent, repeated here so the no-handle path
                        // and this path converge on the same cleanup.
                        server.unregister_ioid(ioid);
                        s.active.lock().take();
                        return Err(MonitorEnd::ChannelClosed);
                    }
                    f = stream.recv() => match f {
                        Some(f) => f,
                        None => {
                            server.unregister_ioid(ioid);
                            s.active.lock().take();
                            return Err(MonitorEnd::ConnectionLost);
                        }
                    },
                }
            }
            // No handle: nothing can cancel this loop, so just await the
            // next frame.
            None => match stream.recv().await {
                Some(f) => f,
                None => {
                    server.unregister_ioid(ioid);
                    return Err(MonitorEnd::ConnectionLost);
                }
            },
        };
        // Decode DATA through the connection `TypeCache` (same cache the
        // MONITOR INIT used above) so `0xFE <slot>` back-references inside
        // `any`/`variant` values resolve, mirroring pvxs `clientmon.cpp:
        // 485-552` which reuses one `rxRegistry` for the INIT type and
        // every DATA value. The lock is scoped to the decode so the
        // non-`Send` parking_lot guard is dropped before the awaits below.
        let decoded = decode_op_response_cached(&frame, Some(&intro), &mut cache.lock());
        match decoded {
            Ok(OpResponse::Data(d)) => {
                // a non-empty overrun bitset means the server
                // coalesced updates because we fell behind. Capture it
                // before `d.value` is moved below.
                let srv_squash = !d.overrun.is_empty();
                let value = if let Some(prev) = prior.as_ref() {
                    crate::pvdata::encode::fill_unmarked_from_prior(
                        &intro, &d.changed, 0, d.value, prev,
                    )
                } else {
                    d.value
                };
                prior = Some(value.clone());
                callback(&intro, &value);
                events_since_ack += 1;
                if let Some(s) = &state {
                    let mut st = s.stats.lock();
                    st.n_delivered += 1;
                    if srv_squash {
                        st.n_srv_squash += 1;
                    }
                    if events_since_ack > st.max_events_per_ack {
                        st.max_events_per_ack = events_since_ack;
                    }
                }
                // (d was destructured above when computing `value`.)
                if flow.pipeline && events_since_ack >= flow.ack_at {
                    let ack = codec.build_monitor_ack(sid, ioid, events_since_ack);
                    if server.send_for_channel(sid, ack).await.is_err() {
                        server.unregister_ioid(ioid);
                        return Err(MonitorEnd::ConnectionLost);
                    }
                    if let Some(s) = &state {
                        s.stats.lock().n_acks += 1;
                    }
                    events_since_ack = 0;
                }
            }
            Ok(OpResponse::Status(s)) => {
                server.unregister_ioid(ioid);
                if let Some(st) = &state {
                    st.active.lock().take();
                }
                if s.status.is_success() {
                    return Ok(());
                } else {
                    return Err(MonitorEnd::Fatal(PvaError::Protocol(format!(
                        "MONITOR error: {:?}",
                        s.status
                    ))));
                }
            }
            Ok(OpResponse::Init(_)) => {
                // A second INIT while the monitor is already Running is a
                // state-machine violation. pvxs accepts only Creating+INIT,
                // Idle+non-INIT, or Running+non-INIT and resets the
                // connection otherwise (clientmon.cpp:568-605). Surface it
                // as fatal — matching the raw path, which classifies
                // unexpected control frames as `RawMonitorFrameKind::Fatal`
                // — instead of treating the violation as harmless.
                server.unregister_ioid(ioid);
                if let Some(s) = &state {
                    s.active.lock().take();
                }
                return Err(MonitorEnd::Fatal(PvaError::Protocol(
                    "MONITOR: unexpected second INIT on a running subscription".into(),
                )));
            }
            Err(e) => {
                // A decode fault on a post-INIT MONITOR frame (truncated
                // DATA body, missing trailing overrun bitset, malformed
                // FINISH Status) is a connection-level protocol fault in
                // pvxs: it logs an invalid MONITOR and resets the
                // connection (clientmon.cpp:601-605). Surface it as fatal
                // instead of logging and skipping under the same IOID —
                // a silent skip also desyncs pipeline ACK accounting (the
                // skipped frame's credit is never returned), which can
                // stall a window-limited server. This mirrors the raw
                // path's `MonitorEnd::Fatal` on a bad classification.
                server.unregister_ioid(ioid);
                if let Some(s) = &state {
                    s.active.lock().take();
                }
                return Err(MonitorEnd::Fatal(PvaError::Protocol(format!(
                    "MONITOR decode error: {e}"
                ))));
            }
        }
    }
}

// ── RPC ────────────────────────────────────────────────────────────────

pub async fn op_rpc(
    channel: &Arc<Channel>,
    request_desc: &FieldDesc,
    request_value: &PvField,
    op_timeout: Duration,
) -> PvaResult<(FieldDesc, PvField)> {
    let (server, sid) = ensure_active_with_op_timeout(channel, op_timeout).await?;
    let order = server.byte_order;
    let big_endian = matches!(order, ByteOrder::Big);
    let codec = PvaCodec { big_endian };
    let ioid = alloc_ioid();

    // pvxs serverget.cpp:369 calls from_wire_type_value (type+value); type-only fails.
    let mut pv_req = Vec::new();
    encode_type_desc(request_desc, order, &mut pv_req);
    encode_pv_field(request_value, request_desc, order, &mut pv_req);

    let mut stream = server.register_ioid_stream(sid, ioid, Command::Rpc.code());
    let mut ioid_guard = IoidGuard::new(server.clone(), ioid);

    // INIT
    let mut init = Vec::with_capacity(9 + pv_req.len());
    init.put_u32(sid, order);
    init.put_u32(ioid, order);
    init.put_u8(QosFlags::INIT);
    init.extend_from_slice(&pv_req);
    let init_h = PvaHeader::application(false, order, Command::Rpc.code(), init.len() as u32);
    let mut init_frame = Vec::with_capacity(8 + init.len());
    init_h.write_into(&mut init_frame);
    init_frame.extend_from_slice(&init);
    server.send_for_channel(sid, init_frame).await?;

    let init_resp_frame = await_frame(&mut stream, op_timeout).await?;
    let init_resp = match decode_op_or_reset(&server, &init_resp_frame, None)? {
        OpResponse::Init(i) => i,
        other => {
            // Wrong response kind for this op step == impossible op state.
            // pvxs `M.fault()`s and `bev.reset()`s (clientget.cpp:456-493).
            server.close();
            server.unregister_ioid(ioid);
            return Err(PvaError::Protocol(format!(
                "expected RPC INIT, got {other:?}"
            )));
        }
    };
    if !init_resp.status.is_success() {
        server.unregister_ioid(ioid);
        return Err(PvaError::Protocol(format!(
            "RPC INIT: {:?}",
            init_resp.status
        )));
    }
    ioid_guard.arm_destroy(sid);
    let response_intro = init_resp.introspection;

    // DATA — RPC argument: `type(arg) + full_value(arg)`.
    // pvxs clientget.cpp:307-311 — `to_wire(R, type); to_wire_full(R, arg)`.
    let mut data = Vec::new();
    data.put_u32(sid, order);
    data.put_u32(ioid, order);
    data.put_u8(0x00);
    crate::pvdata::encode::encode_type_desc(request_desc, order, &mut data);
    encode_pv_field(request_value, request_desc, order, &mut data);
    let data_h = PvaHeader::application(false, order, Command::Rpc.code(), data.len() as u32);
    let mut data_frame = Vec::with_capacity(8 + data.len());
    data_h.write_into(&mut data_frame);
    data_frame.extend_from_slice(&data);
    server.send_for_channel(sid, data_frame).await?;

    let resp_frame = await_frame(&mut stream, op_timeout).await?;
    // RPC response carries its own type — `response_intro` from INIT is
    // unused (RPC INIT has no introspection per pvxs).
    let _ = response_intro;
    let result = match decode_op_or_reset(&server, &resp_frame, None)? {
        OpResponse::Data(d) => {
            if d.status.is_success() {
                let desc = d.response_desc.unwrap_or(FieldDesc::Variant);
                Ok((desc, d.value))
            } else {
                Err(PvaError::Protocol(format!("RPC: {:?}", d.status)))
            }
        }
        OpResponse::Status(s) => Err(PvaError::Protocol(format!("RPC: {:?}", s.status))),
        other => {
            // Wrong response kind for the RPC data step == impossible op
            // state → connection-fatal (pvxs clientget.cpp:456-493).
            server.close();
            Err(PvaError::Protocol(format!(
                "expected RPC data, got {other:?}"
            )))
        }
    };

    ioid_guard.disarm();
    let destroy = codec.build_destroy_request(sid, ioid);
    let _ = server.send_for_channel(sid, destroy).await;
    server.unregister_ioid(ioid);
    result
}

// ── PUT_GET (cmd 12) ────────────────────────────────────────────────────

/// PVA `PUT_GET` (cmd 12) — atomic put-then-get round trip.
///
/// Puts `value_str` to the channel's `.value` field, then receives the
/// (possibly post-processed) value back, all in one operation. The
/// wire lifecycle mirrors the GET / PUT ops:
///
/// 1. INIT (`subcmd 0x08`): send the pvRequest; server replies with
///    `status + putIF + getIF` (two type descriptors).
/// 2. PUT-GET (`subcmd 0x00`): send `put bitset + put value`; server
///    applies the put then replies `status + get bitset + get value`.
/// 3. DESTROY (`subcmd 0x10`): release the op.
///
/// pvxs leaves `handle_PUT_GET` empty; the Rust server implements the
/// full operation (see `server_native::tcp::handle_put_get`).
pub async fn op_put_get(
    channel: &Arc<Channel>,
    value_str: &str,
    op_timeout: Duration,
) -> PvaResult<(FieldDesc, PvField)> {
    let (server, sid) = ensure_active_with_op_timeout(channel, op_timeout).await?;
    let order = server.byte_order;
    let big_endian = matches!(order, ByteOrder::Big);
    let codec = PvaCodec { big_endian };
    let ioid = alloc_ioid();

    let pv_req = build_pv_request_value_only(big_endian);
    let mut stream = server.register_ioid_stream(sid, ioid, Command::PutGet.code());
    let mut ioid_guard = IoidGuard::new(server.clone(), ioid);
    let cache = server.type_cache();

    // INIT — `sid + ioid + 0x08 + pvRequest`.
    let mut init = Vec::with_capacity(9 + pv_req.len());
    init.put_u32(sid, order);
    init.put_u32(ioid, order);
    init.put_u8(QosFlags::INIT);
    init.extend_from_slice(&pv_req);
    let init_h = PvaHeader::application(false, order, Command::PutGet.code(), init.len() as u32);
    let mut init_frame = Vec::with_capacity(8 + init.len());
    init_h.write_into(&mut init_frame);
    init_frame.extend_from_slice(&init);
    server.send_for_channel(sid, init_frame).await?;

    let init_resp = await_frame(&mut stream, op_timeout).await?;
    let intro = match decode_put_get_init(&init_resp, &mut cache.lock()) {
        Ok(Ok(intro)) => intro,
        Ok(Err(status)) => {
            server.unregister_ioid(ioid);
            return Err(PvaError::Protocol(format!(
                "PUT_GET INIT failed: {status:?}"
            )));
        }
        Err(e) => {
            // Command/subcommand mismatch or truncated INIT body is fatal.
            server.close();
            return Err(e);
        }
    };
    ioid_guard.arm_destroy(sid);

    // Build the value against the negotiated introspection.
    let value = build_put_value(&intro, value_str)?;

    // PUT-GET data — `sid + ioid + 0x00 + put bitset + put value`.
    let mut data = Vec::new();
    data.put_u32(sid, order);
    data.put_u32(ioid, order);
    data.put_u8(0x00);
    let mut changed = BitSet::new();
    if let Some(bit) = intro.bit_for_path("value") {
        changed.set(bit);
    } else {
        changed.set(0);
    }
    changed.write_into(order, &mut data);
    // pvxs `from_wire_valid` decodes a BitSet delta — only the fields
    // whose bit is set. Encode consistently.
    encode_pv_field_with_bitset(&value, &intro, &changed, 0, order, &mut data);
    let data_h = PvaHeader::application(false, order, Command::PutGet.code(), data.len() as u32);
    let mut data_frame = Vec::with_capacity(8 + data.len());
    data_h.write_into(&mut data_frame);
    data_frame.extend_from_slice(&data);
    server.send_for_channel(sid, data_frame).await?;

    let resp_frame = await_frame(&mut stream, op_timeout).await?;
    let result = match decode_put_get_data(&resp_frame, &intro, &mut cache.lock()) {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(status)) => Err(PvaError::Protocol(format!("PUT_GET: {status:?}"))),
        Err(e) => {
            // Command mismatch or truncated data body is fatal.
            server.close();
            Err(e)
        }
    };

    ioid_guard.disarm();
    let destroy = codec.build_destroy_request(sid, ioid);
    let _ = server.send_for_channel(sid, destroy).await;
    server.unregister_ioid(ioid);
    result.map(|v| (intro, v))
}

/// Decode a `PUT_GET` INIT response: `ioid + subcmd + status + putIF +
/// getIF`. On success returns the get-leg introspection (used to encode
/// the put value and decode the readback).
///
/// The two-level result separates connection-fatal faults from per-op
/// failures: an outer `Err` (command/subcommand mismatch, truncated body)
/// is a protocol violation pvxs answers with `bev.reset()`
/// (clientget.cpp:456-493); an inner `Ok(Err(status))` is a non-success
/// INIT — a per-operation error the caller surfaces without resetting.
fn decode_put_get_init(
    frame: &super::decode::Frame,
    type_cache: &mut crate::pvdata::encode::TypeCache,
) -> PvaResult<Result<FieldDesc, crate::proto::Status>> {
    if frame.header.command != Command::PutGet.code() {
        return Err(PvaError::Protocol(format!(
            "expected PUT_GET INIT, got command {}",
            frame.header.command
        )));
    }
    let order = frame.order();
    let mut cur = frame.cursor();
    let _ioid = cur
        .get_u32(order)
        .map_err(|e| PvaError::Decode(e.to_string()))?;
    let subcmd = cur.get_u8().map_err(|e| PvaError::Decode(e.to_string()))?;
    if subcmd & QosFlags::INIT == 0 {
        return Err(PvaError::Protocol(format!(
            "expected PUT_GET INIT subcmd, got 0x{subcmd:02x}"
        )));
    }
    let status = crate::proto::Status::decode(&mut cur, order)
        .map_err(|e| PvaError::Decode(e.to_string()))?;
    if !status.is_success() {
        return Ok(Err(status));
    }
    // putIF then getIF. The put structure is decoded (advancing the
    // cursor + populating the type cache) but the get structure is
    // what the data legs use.
    let _put_if = crate::pvdata::encode::decode_type_desc_cached(&mut cur, order, type_cache)
        .map_err(|e| PvaError::Decode(e.to_string()))?;
    let get_if = crate::pvdata::encode::decode_type_desc_cached(&mut cur, order, type_cache)
        .map_err(|e| PvaError::Decode(e.to_string()))?;
    Ok(Ok(get_if))
}

/// Decode a `PUT_GET` data response: `ioid + subcmd + status + get
/// bitset + get value`.
///
/// Same two-level result as [`decode_put_get_init`]: an outer `Err`
/// (command mismatch, truncated body) is connection-fatal; an inner
/// `Ok(Err(status))` is a non-success per-op result.
fn decode_put_get_data(
    frame: &super::decode::Frame,
    intro: &FieldDesc,
    type_cache: &mut crate::pvdata::encode::TypeCache,
) -> PvaResult<Result<PvField, crate::proto::Status>> {
    if frame.header.command != Command::PutGet.code() {
        return Err(PvaError::Protocol(format!(
            "expected PUT_GET data, got command {}",
            frame.header.command
        )));
    }
    let order = frame.order();
    let mut cur = frame.cursor();
    let _ioid = cur
        .get_u32(order)
        .map_err(|e| PvaError::Decode(e.to_string()))?;
    let _subcmd = cur.get_u8().map_err(|e| PvaError::Decode(e.to_string()))?;
    let status = crate::proto::Status::decode(&mut cur, order)
        .map_err(|e| PvaError::Decode(e.to_string()))?;
    if !status.is_success() {
        return Ok(Err(status));
    }
    let changed = BitSet::decode(&mut cur, order).map_err(|e| PvaError::Decode(e.to_string()))?;
    let value = crate::pvdata::encode::decode_pv_field_with_bitset_cached(
        intro, &changed, 0, &mut cur, order, type_cache,
    )
    .map_err(|e| PvaError::Decode(e.to_string()))?;
    Ok(Ok(value))
}

// ── PROCESS (cmd 16) ────────────────────────────────────────────────────

/// Build a PROCESS INIT frame (`sid + ioid + 0x08 + pvRequest`) carrying
/// the caller-supplied `pv_req` bytes verbatim. Factored out so the
/// caller's request can be verified at the wire level (PVA-RS-2026-05-28-62
/// regression).
fn build_process_init(sid: u32, ioid: u32, pv_req: &[u8], order: ByteOrder) -> Vec<u8> {
    let mut init = Vec::with_capacity(9 + pv_req.len());
    init.put_u32(sid, order);
    init.put_u32(ioid, order);
    init.put_u8(QosFlags::INIT);
    init.extend_from_slice(pv_req);
    let init_h = PvaHeader::application(false, order, Command::Process.code(), init.len() as u32);
    let mut init_frame = Vec::with_capacity(8 + init.len());
    init_h.write_into(&mut init_frame);
    init_frame.extend_from_slice(&init);
    init_frame
}

/// PVA `PROCESS` (cmd 16) — trigger record processing without
/// transferring a value.
///
/// Wire lifecycle:
/// 1. INIT (`subcmd 0x08`): send the pvRequest; server replies
///    `status` (no introspection — there is no value type).
/// 2. PROCESS (`subcmd 0x00`): no payload; server runs the processing
///    hook and replies `status`.
/// 3. DESTROY (`subcmd 0x10`): release the op.
///
/// The empty default request — `[`sentinel_all_fields`]` — matches EPICS
/// base pvaClient `createProcess("")` (pvaClientChannel.cpp:316-333), whose
/// parsed empty pvRequest is serialized into PROCESS INIT
/// (clientContextImpl.cpp:528-536) and handed to the provider's
/// `createChannelProcess` (responseHandlers.cpp:2556-2561). Use
/// [`op_process_with_request`] to send a provider-specific request.
pub async fn op_process(channel: &Arc<Channel>, op_timeout: Duration) -> PvaResult<()> {
    op_process_with_request(channel, sentinel_all_fields(), op_timeout).await
}

/// `op_process` variant that sends a caller-supplied PROCESS pvRequest
/// (e.g. `record[block=true]` or a provider-specific option set) instead of
/// the empty default. pvAccess serializes this request on PROCESS INIT and
/// the provider can inspect it during `createChannelProcess`
/// (responseHandlers.cpp:2504-2511). Mirrors pvaClient
/// `PvaClientChannel::createProcess(pvRequest)` (pvaClientProcess.cpp:198-200).
pub async fn op_process_with_request(
    channel: &Arc<Channel>,
    pv_req: &[u8],
    op_timeout: Duration,
) -> PvaResult<()> {
    let (server, sid) = ensure_active_with_op_timeout(channel, op_timeout).await?;
    let order = server.byte_order;
    let big_endian = matches!(order, ByteOrder::Big);
    let codec = PvaCodec { big_endian };
    let ioid = alloc_ioid();

    let mut stream = server.register_ioid_stream(sid, ioid, Command::Process.code());
    let mut ioid_guard = IoidGuard::new(server.clone(), ioid);

    // INIT — `sid + ioid + 0x08 + pvRequest`.
    let init_frame = build_process_init(sid, ioid, pv_req, order);
    server.send_for_channel(sid, init_frame).await?;

    let init_resp = await_frame(&mut stream, op_timeout).await?;
    let init_status = match decode_process_status(&init_resp) {
        Ok(s) => s,
        Err(e) => {
            // Decode fault / command mismatch on the INIT reply is fatal.
            server.close();
            return Err(e);
        }
    };
    if !init_status.is_success() {
        server.unregister_ioid(ioid);
        return Err(PvaError::Protocol(format!(
            "PROCESS INIT failed: {init_status:?}"
        )));
    }
    ioid_guard.arm_destroy(sid);

    // PROCESS data — `sid + ioid + 0x00`, no payload.
    let mut data = Vec::with_capacity(9);
    data.put_u32(sid, order);
    data.put_u32(ioid, order);
    data.put_u8(0x00);
    let data_h = PvaHeader::application(false, order, Command::Process.code(), data.len() as u32);
    let mut data_frame = Vec::with_capacity(8 + data.len());
    data_h.write_into(&mut data_frame);
    data_frame.extend_from_slice(&data);
    server.send_for_channel(sid, data_frame).await?;

    let resp_frame = await_frame(&mut stream, op_timeout).await?;
    let result = match decode_process_status(&resp_frame) {
        Ok(s) if s.is_success() => Ok(()),
        Ok(s) => Err(PvaError::Protocol(format!("PROCESS failed: {s:?}"))),
        Err(e) => {
            // Decode fault / command mismatch on the done reply is fatal.
            server.close();
            Err(e)
        }
    };

    ioid_guard.disarm();
    let destroy = codec.build_destroy_request(sid, ioid);
    let _ = server.send_for_channel(sid, destroy).await;
    server.unregister_ioid(ioid);
    result
}

/// Decode a `PROCESS` response: `ioid + subcmd + status`. Returns the
/// decoded `Status`.
///
/// An `Err` from this decoder is always connection-fatal — a command
/// mismatch or a truncated body is a protocol violation that pvxs answers
/// with `bev.reset()` (clientget.cpp:456-493). A non-success `Status`
/// decodes to `Ok`: it is a per-operation result, NOT a fault, so the
/// caller surfaces it without resetting the circuit.
fn decode_process_status(frame: &super::decode::Frame) -> PvaResult<crate::proto::Status> {
    if frame.header.command != Command::Process.code() {
        return Err(PvaError::Protocol(format!(
            "expected PROCESS response, got command {}",
            frame.header.command
        )));
    }
    let order = frame.order();
    let mut cur = frame.cursor();
    let _ioid = cur
        .get_u32(order)
        .map_err(|e| PvaError::Decode(e.to_string()))?;
    let _subcmd = cur.get_u8().map_err(|e| PvaError::Decode(e.to_string()))?;
    let status = crate::proto::Status::decode(&mut cur, order)
        .map_err(|e| PvaError::Decode(e.to_string()))?;
    Ok(status)
}

// ── Helpers ────────────────────────────────────────────────────────────

async fn await_frame(
    stream: &mut mpsc::UnboundedReceiver<super::decode::Frame>,
    op_timeout: Duration,
) -> PvaResult<super::decode::Frame> {
    let frame = timeout(op_timeout, stream.recv())
        .await
        .map_err(|_| PvaError::Timeout)?
        .ok_or_else(|| PvaError::Protocol("connection closed".into()))?;
    Ok(frame)
}

/// Single-shot variant of [`await_frame`] for the new TwoShot ioid
/// router path. Avoids the per-op `unbounded_channel` allocation —
/// the reader task pops oneshots FIFO from `IoidSlot::TwoShot`'s
/// VecDeque, so each pipelined GET / PUT response lands directly on
/// a stack-allocated oneshot future.
async fn await_oneshot_frame(
    rx: tokio::sync::oneshot::Receiver<super::decode::Frame>,
    op_timeout: Duration,
) -> PvaResult<super::decode::Frame> {
    timeout(op_timeout, rx)
        .await
        .map_err(|_| PvaError::Timeout)?
        .map_err(|_| PvaError::Protocol("connection closed".into()))
}

fn sentinel_all_fields() -> &'static [u8] {
    &[0xFD, 0x02, 0x00, 0x80, 0x00, 0x00]
}

/// Build a PUT value where only `field_path` (e.g. `"alarm.severity"`)
/// carries the parsed value; every other field gets a default. Mirrors
/// pvxs `PutBuilder::set("alarm.severity", val)` semantics — the
/// matching changed-bitset must be built separately via
/// [`crate::pvdata::FieldDesc::bit_for_path`].
fn build_put_value_for_path(
    desc: &FieldDesc,
    field_path: &[&str],
    value_str: &str,
) -> PvaResult<PvField> {
    if field_path.is_empty() {
        // Targeting the root: parse value directly into the descriptor
        // shape (recurses into the "value" subfield convention used by
        // build_put_value for compatibility).
        return build_put_value(desc, value_str);
    }
    match desc {
        FieldDesc::Structure { fields, struct_id } => {
            let head = field_path[0];
            let tail = &field_path[1..];
            let mut s = PvStructure::new(struct_id);
            for (name, child) in fields {
                if name == head {
                    s.fields.push((
                        name.clone(),
                        build_put_value_for_path(child, tail, value_str)?,
                    ));
                } else {
                    s.fields.push((
                        name.clone(),
                        crate::pvdata::encode::default_value_for(child),
                    ));
                }
            }
            // Path didn't match any field → clear failure.
            if !fields.iter().any(|(n, _)| n == head) {
                return Err(PvaError::InvalidValue(format!(
                    "field '{head}' not present in target structure"
                )));
            }
            Ok(PvField::Structure(s))
        }
        FieldDesc::Scalar(st) if field_path.is_empty() => ScalarValue::parse(*st, value_str)
            .map(PvField::Scalar)
            .map_err(PvaError::InvalidValue),
        FieldDesc::ScalarArray(st) if field_path.is_empty() => {
            let mut items = Vec::new();
            for tok in value_str
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                items.push(ScalarValue::parse(*st, tok).map_err(PvaError::InvalidValue)?);
            }
            Ok(PvField::ScalarArray(items))
        }
        _ => Err(PvaError::InvalidValue(format!(
            "cannot navigate path through {desc} (remaining: {field_path:?})"
        ))),
    }
}

fn build_put_value(desc: &FieldDesc, value_str: &str) -> PvaResult<PvField> {
    match desc {
        FieldDesc::Scalar(st) => ScalarValue::parse(*st, value_str)
            .map(PvField::Scalar)
            .map_err(PvaError::InvalidValue),
        FieldDesc::ScalarArray(st) => {
            let mut items = Vec::new();
            for tok in value_str
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                items.push(ScalarValue::parse(*st, tok).map_err(PvaError::InvalidValue)?);
            }
            Ok(PvField::ScalarArray(items))
        }
        FieldDesc::Structure { fields, struct_id } => {
            let mut s = PvStructure::new(struct_id);
            for (name, child) in fields {
                if name == "value" {
                    s.fields
                        .push((name.clone(), build_put_value(child, value_str)?));
                } else {
                    s.fields.push((
                        name.clone(),
                        crate::pvdata::encode::default_value_for(child),
                    ));
                }
            }
            Ok(PvField::Structure(s))
        }
        FieldDesc::Union { variants, .. } => build_put_union(variants, value_str),
        FieldDesc::UnionArray { variants, .. } => {
            // Element-per-token: each `;`-separated token becomes one
            // union element built via `build_put_union`. An empty input
            // is a legal zero-length array.
            let mut items = Vec::new();
            for tok in value_str
                .split(';')
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                let elem = build_put_union(variants, tok)?;
                match elem {
                    PvField::Union {
                        selector,
                        variant_name,
                        value,
                    } => items.push(Some(UnionItem {
                        selector,
                        variant_name,
                        value: *value,
                    })),
                    other => {
                        return Err(PvaError::InvalidValue(format!(
                            "internal: build_put_union yielded {other:?}"
                        )));
                    }
                }
            }
            Ok(PvField::UnionArray(items))
        }
        FieldDesc::Variant => build_put_variant(value_str),
        FieldDesc::VariantArray => {
            // Comma-separated tokens, each inferred independently.
            let mut items = Vec::new();
            for tok in value_str
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                match build_put_variant(tok)? {
                    PvField::Variant(vv) => items.push(Some(*vv)),
                    other => {
                        return Err(PvaError::InvalidValue(format!(
                            "internal: build_put_variant yielded {other:?}"
                        )));
                    }
                }
            }
            Ok(PvField::VariantArray(items))
        }
        FieldDesc::StructureArray { struct_id, fields } => {
            // Each `;`-separated token builds one element structure; the
            // token is routed into the element's `value` field (or, if
            // there is none, its first scalar leaf) exactly like the
            // scalar `Structure` arm above. An empty input yields a
            // zero-length array.
            let mut items = Vec::new();
            for tok in value_str
                .split(';')
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                let element_desc = FieldDesc::Structure {
                    struct_id: struct_id.clone(),
                    fields: fields.clone(),
                };
                match build_put_struct_element(&element_desc, tok)? {
                    PvField::Structure(s) => items.push(Some(s)),
                    other => {
                        return Err(PvaError::InvalidValue(format!(
                            "internal: structure-array element built as {other:?}"
                        )));
                    }
                }
            }
            Ok(PvField::StructureArray(items))
        }
    }
}

/// Build a union PUT value. The variant is selected either
/// explicitly — `value_str` of the form `variantName=payload` (or
/// `variantName:payload`) — or implicitly by picking the first variant
/// whose descriptor the bare payload parses against. An empty
/// `value_str` produces the null union (`selector == -1`).
fn build_put_union(variants: &[(String, FieldDesc)], value_str: &str) -> PvaResult<PvField> {
    let trimmed = value_str.trim();
    if trimmed.is_empty() {
        return Ok(PvField::Union {
            selector: -1,
            variant_name: String::new(),
            value: Box::new(PvField::Null),
        });
    }

    // Explicit `name=payload` / `name:payload` selection. The split
    // point is the first `=` or `:`; a leading variant name that itself
    // contains neither is required for the explicit form to engage.
    let explicit = trimmed
        .find(['=', ':'])
        .map(|i| (trimmed[..i].trim(), trimmed[i + 1..].trim()));
    if let Some((name, payload)) = explicit {
        if let Some(idx) = variants.iter().position(|(n, _)| n == name) {
            let (_, vdesc) = &variants[idx];
            let value = build_put_value(vdesc, payload)?;
            return Ok(PvField::Union {
                selector: idx as i32,
                variant_name: name.to_string(),
                value: Box::new(value),
            });
        }
        // Name not found — fall through to implicit matching against the
        // whole `value_str` so e.g. a timestamp "1:2:3" still parses.
    }

    // Implicit: first variant the bare payload builds against cleanly.
    for (idx, (name, vdesc)) in variants.iter().enumerate() {
        if let Ok(value) = build_put_value(vdesc, trimmed) {
            return Ok(PvField::Union {
                selector: idx as i32,
                variant_name: name.clone(),
                value: Box::new(value),
            });
        }
    }
    Err(PvaError::InvalidValue(format!(
        "value '{value_str}' does not match any union variant; \
         use 'variantName=value' to select explicitly"
    )))
}

/// Build a variant ("any") PUT value. The carried scalar type is
/// inferred from the textual form — narrowest type wins: `bool` →
/// `i64` → `f64` → `String`. An empty `value_str` produces the null
/// variant (no embedded descriptor).
fn build_put_variant(value_str: &str) -> PvaResult<PvField> {
    let trimmed = value_str.trim();
    if trimmed.is_empty() {
        return Ok(PvField::Variant(Box::new(VariantValue {
            desc: None,
            value: PvField::Null,
        })));
    }
    let (st, sv) = if trimmed.eq_ignore_ascii_case("true") {
        (ScalarType::Boolean, ScalarValue::Boolean(true))
    } else if trimmed.eq_ignore_ascii_case("false") {
        (ScalarType::Boolean, ScalarValue::Boolean(false))
    } else if let Ok(i) = trimmed.parse::<i64>() {
        (ScalarType::Long, ScalarValue::Long(i))
    } else if let Ok(d) = trimmed.parse::<f64>() {
        (ScalarType::Double, ScalarValue::Double(d))
    } else {
        (ScalarType::String, ScalarValue::String(trimmed.to_string()))
    };
    Ok(PvField::Variant(Box::new(VariantValue {
        desc: Some(FieldDesc::Scalar(st)),
        value: PvField::Scalar(sv),
    })))
}

/// Build one element of a structure array. The token is routed
/// into the element's `value` field if present, otherwise into its
/// first scalar / scalar-array leaf; every other field is default-
/// filled. Distinct from [`build_put_value`]'s `Structure` arm only in
/// the fallback-to-first-scalar-leaf behaviour, which matters because
/// structure-array elements are frequently plain `{ value: scalar }`
/// records without an NT wrapper.
fn build_put_struct_element(desc: &FieldDesc, value_str: &str) -> PvaResult<PvField> {
    let FieldDesc::Structure { fields, struct_id } = desc else {
        return build_put_value(desc, value_str);
    };
    // Prefer a field literally named "value"; else the first scalar or
    // scalar-array leaf.
    let target = fields.iter().position(|(n, _)| n == "value").or_else(|| {
        fields
            .iter()
            .position(|(_, d)| matches!(d, FieldDesc::Scalar(_) | FieldDesc::ScalarArray(_)))
    });
    let mut s = PvStructure::new(struct_id);
    for (idx, (name, child)) in fields.iter().enumerate() {
        if Some(idx) == target {
            s.fields
                .push((name.clone(), build_put_value(child, value_str)?));
        } else {
            s.fields.push((
                name.clone(),
                crate::pvdata::encode::default_value_for(child),
            ));
        }
    }
    Ok(PvField::Structure(s))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pvdata::encode::{decode_pv_field, encode_pv_field};
    use std::io::Cursor;

    /// PVA-RS-2026-05-28-62: the PROCESS INIT frame must carry the
    /// caller-supplied pvRequest verbatim (after the 9-byte
    /// `sid + ioid + subcmd` prefix), not a hard-coded `field(value)`
    /// request. Mirrors pvAccess serializing the stored PROCESS pvRequest
    /// on INIT (clientContextImpl.cpp:528-536).
    #[test]
    fn process_init_embeds_caller_pv_request() {
        let order = ByteOrder::Little;
        let sid = 0x11223344u32;
        let ioid = 0x55667788u32;
        // A distinctive caller request the value-only default never produces.
        let pv_req: &[u8] = &[
            0xFD, 0x09, 0x00, 0x80, 0x01, 0x07, b'r', b'e', b'c', b'o', b'r', b'd', 0x00,
        ];

        let frame = build_process_init(sid, ioid, pv_req, order);
        // header (8) + sid (4) + ioid (4) + subcmd (1) = 17-byte prefix.
        let body = &frame[8..];
        let mut cur = Cursor::new(body);
        assert_eq!(cur.get_u32(order).unwrap(), sid);
        assert_eq!(cur.get_u32(order).unwrap(), ioid);
        assert_eq!(cur.get_u8().unwrap(), QosFlags::INIT);
        assert_eq!(
            &body[9..],
            pv_req,
            "PROCESS INIT must contain caller request"
        );

        // And it must differ from the empty/value-only defaults — i.e. the
        // request is genuinely caller-controlled, not ignored.
        let default_frame = build_process_init(sid, ioid, sentinel_all_fields(), order);
        assert_ne!(frame, default_frame);
        let value_only = build_pv_request_value_only(false);
        let value_only_frame = build_process_init(sid, ioid, &value_only, order);
        assert_ne!(frame, value_only_frame);
    }

    /// Round-trip a built PUT value through encode/decode against its
    /// descriptor — proves the value built by `build_put_value` is
    /// wire-valid, not merely well-typed.
    fn assert_round_trips(desc: &FieldDesc, value: &PvField) {
        for order in [ByteOrder::Little, ByteOrder::Big] {
            let mut buf = Vec::new();
            encode_pv_field(value, desc, order, &mut buf);
            let mut cur = Cursor::new(buf.as_slice());
            let decoded = decode_pv_field(desc, &mut cur, order)
                .unwrap_or_else(|e| panic!("decode failed ({order:?}): {e:?}"));
            assert_eq!(
                cur.position() as usize,
                buf.len(),
                "trailing bytes after decode"
            );
            assert_eq!(
                format!("{decoded}"),
                format!("{value}"),
                "round-trip mismatch order={order:?}"
            );
        }
    }

    fn union_desc() -> FieldDesc {
        FieldDesc::Union {
            struct_id: String::new(),
            variants: vec![
                ("intValue".into(), FieldDesc::Scalar(ScalarType::Int)),
                ("doubleValue".into(), FieldDesc::Scalar(ScalarType::Double)),
                ("stringValue".into(), FieldDesc::Scalar(ScalarType::String)),
            ],
        }
    }

    #[test]
    fn put_union_explicit_variant_selection() {
        let desc = union_desc();
        let v = build_put_value(&desc, "doubleValue=2.5").unwrap();
        match &v {
            PvField::Union {
                selector,
                variant_name,
                value,
            } => {
                assert_eq!(*selector, 1);
                assert_eq!(variant_name, "doubleValue");
                assert_eq!(**value, PvField::Scalar(ScalarValue::Double(2.5)));
            }
            other => panic!("expected union, got {other:?}"),
        }
        assert_round_trips(&desc, &v);
    }

    #[test]
    fn put_union_explicit_colon_form() {
        let desc = union_desc();
        let v = build_put_value(&desc, "stringValue:hello world").unwrap();
        match &v {
            PvField::Union {
                selector, value, ..
            } => {
                assert_eq!(*selector, 2);
                assert_eq!(
                    **value,
                    PvField::Scalar(ScalarValue::String("hello world".into()))
                );
            }
            other => panic!("expected union, got {other:?}"),
        }
        assert_round_trips(&desc, &v);
    }

    #[test]
    fn put_union_implicit_first_matching_variant() {
        let desc = union_desc();
        // "7" parses as Int (the first variant) — selector 0.
        let v = build_put_value(&desc, "7").unwrap();
        match &v {
            PvField::Union { selector, .. } => assert_eq!(*selector, 0),
            other => panic!("expected union, got {other:?}"),
        }
        assert_round_trips(&desc, &v);
    }

    #[test]
    fn put_union_empty_is_null() {
        let desc = union_desc();
        let v = build_put_value(&desc, "").unwrap();
        assert!(matches!(v, PvField::Union { selector: -1, .. }));
        assert_round_trips(&desc, &v);
    }

    #[test]
    fn put_union_unknown_name_falls_back_to_implicit() {
        let desc = union_desc();
        // "1:2:3" — no variant named "1"; the bare string matches the
        // String variant via implicit fallback.
        let v = build_put_value(&desc, "1:2:3").unwrap();
        match &v {
            PvField::Union {
                selector, value, ..
            } => {
                assert_eq!(*selector, 2, "string variant");
                assert_eq!(
                    **value,
                    PvField::Scalar(ScalarValue::String("1:2:3".into()))
                );
            }
            other => panic!("expected union, got {other:?}"),
        }
        assert_round_trips(&desc, &v);
    }

    #[test]
    fn put_variant_infers_scalar_type() {
        for (input, expect_st, expect_val) in [
            ("true", ScalarType::Boolean, ScalarValue::Boolean(true)),
            ("-42", ScalarType::Long, ScalarValue::Long(-42)),
            ("3.5", ScalarType::Double, ScalarValue::Double(3.5)),
            (
                "text",
                ScalarType::String,
                ScalarValue::String("text".into()),
            ),
        ] {
            let v = build_put_value(&FieldDesc::Variant, input).unwrap();
            match &v {
                PvField::Variant(vv) => {
                    assert_eq!(vv.desc, Some(FieldDesc::Scalar(expect_st)), "input {input}");
                    assert_eq!(vv.value, PvField::Scalar(expect_val), "input {input}");
                }
                other => panic!("expected variant, got {other:?}"),
            }
            assert_round_trips(&FieldDesc::Variant, &v);
        }
    }

    #[test]
    fn put_variant_empty_is_null() {
        let v = build_put_value(&FieldDesc::Variant, "").unwrap();
        match &v {
            PvField::Variant(vv) => assert!(vv.desc.is_none()),
            other => panic!("expected variant, got {other:?}"),
        }
        assert_round_trips(&FieldDesc::Variant, &v);
    }

    #[test]
    fn put_structure_array_elements() {
        let desc = FieldDesc::StructureArray {
            struct_id: "elem_t".into(),
            fields: vec![
                ("value".into(), FieldDesc::Scalar(ScalarType::Int)),
                ("flag".into(), FieldDesc::Scalar(ScalarType::Boolean)),
            ],
        };
        let v = build_put_value(&desc, "10; 20; 30").unwrap();
        match &v {
            PvField::StructureArray(items) => {
                assert_eq!(items.len(), 3);
                assert_eq!(
                    items[1].as_ref().unwrap().get_field("value"),
                    Some(&PvField::Scalar(ScalarValue::Int(20)))
                );
            }
            other => panic!("expected structure array, got {other:?}"),
        }
        assert_round_trips(&desc, &v);
    }

    #[test]
    fn put_structure_array_first_scalar_leaf_when_no_value_field() {
        // Element struct has no field named "value"; the token routes
        // into the first scalar leaf — here "n", declared before the
        // non-scalar nested struct.
        let desc = FieldDesc::StructureArray {
            struct_id: "pair_t".into(),
            fields: vec![
                (
                    "meta".into(),
                    FieldDesc::Structure {
                        struct_id: "m_t".into(),
                        fields: vec![("tag".into(), FieldDesc::Scalar(ScalarType::String))],
                    },
                ),
                ("n".into(), FieldDesc::Scalar(ScalarType::Int)),
            ],
        };
        let v = build_put_value(&desc, "5; 6").unwrap();
        match &v {
            PvField::StructureArray(items) => {
                assert_eq!(items.len(), 2);
                assert_eq!(
                    items[0].as_ref().unwrap().get_field("n"),
                    Some(&PvField::Scalar(ScalarValue::Int(5)))
                );
                assert_eq!(
                    items[1].as_ref().unwrap().get_field("n"),
                    Some(&PvField::Scalar(ScalarValue::Int(6)))
                );
            }
            other => panic!("expected structure array, got {other:?}"),
        }
        assert_round_trips(&desc, &v);
    }

    #[test]
    fn put_structure_array_empty() {
        let desc = FieldDesc::StructureArray {
            struct_id: "elem_t".into(),
            fields: vec![("value".into(), FieldDesc::Scalar(ScalarType::Int))],
        };
        let v = build_put_value(&desc, "").unwrap();
        assert!(matches!(&v, PvField::StructureArray(items) if items.is_empty()));
        assert_round_trips(&desc, &v);
    }

    #[test]
    fn put_union_array_elements() {
        let desc = FieldDesc::UnionArray {
            struct_id: String::new(),
            variants: vec![
                ("i".into(), FieldDesc::Scalar(ScalarType::Int)),
                ("s".into(), FieldDesc::Scalar(ScalarType::String)),
            ],
        };
        let v = build_put_value(&desc, "i=1; s=hi; 2").unwrap();
        match &v {
            PvField::UnionArray(items) => {
                assert_eq!(items.len(), 3);
                assert_eq!(items[0].as_ref().unwrap().selector, 0);
                assert_eq!(items[1].as_ref().unwrap().selector, 1);
                assert_eq!(
                    items[2].as_ref().unwrap().selector,
                    0,
                    "bare '2' matches Int variant"
                );
            }
            other => panic!("expected union array, got {other:?}"),
        }
        assert_round_trips(&desc, &v);
    }

    #[test]
    fn put_variant_array_elements() {
        let desc = FieldDesc::VariantArray;
        let v = build_put_value(&desc, "1, 2.5, hello").unwrap();
        match &v {
            PvField::VariantArray(items) => {
                assert_eq!(items.len(), 3);
                assert_eq!(
                    items[0].as_ref().unwrap().desc,
                    Some(FieldDesc::Scalar(ScalarType::Long))
                );
                assert_eq!(
                    items[1].as_ref().unwrap().desc,
                    Some(FieldDesc::Scalar(ScalarType::Double))
                );
                assert_eq!(
                    items[2].as_ref().unwrap().desc,
                    Some(FieldDesc::Scalar(ScalarType::String))
                );
            }
            other => panic!("expected variant array, got {other:?}"),
        }
        assert_round_trips(&desc, &v);
    }

    #[test]
    fn put_union_value_field_inside_structure() {
        // NT-style wrapper: { value: union, alarm: ... } — build_put_value
        // recurses into the `value` field which is itself a union.
        let desc = FieldDesc::Structure {
            struct_id: "epics:nt/NTUnion:1.0".into(),
            fields: vec![
                ("value".into(), union_desc()),
                (
                    "alarm".into(),
                    FieldDesc::Structure {
                        struct_id: "alarm_t".into(),
                        fields: vec![("severity".into(), FieldDesc::Scalar(ScalarType::Int))],
                    },
                ),
            ],
        };
        let v = build_put_value(&desc, "intValue=99").unwrap();
        match &v {
            PvField::Structure(s) => match s.get_field("value") {
                Some(PvField::Union {
                    selector, value, ..
                }) => {
                    assert_eq!(*selector, 0);
                    assert_eq!(**value, PvField::Scalar(ScalarValue::Int(99)));
                }
                other => panic!("expected union in value field, got {other:?}"),
            },
            other => panic!("expected structure, got {other:?}"),
        }
        assert_round_trips(&desc, &v);
    }

    /// Regression: the PUT EXEC DATA frame must encode a BitSet *delta*
    /// — only the fields whose bit is set — so a pvxs server's
    /// `from_wire_valid` (serverget.cpp:451) decodes the exact bytes the
    /// client emitted. Previously the BitSet marked only `value` while
    /// `encode_pv_field` wrote the *full* NT structure, desyncing the
    /// server-side decode.
    #[test]
    fn put_data_frame_bitset_matches_encoded_field_set() {
        use crate::pvdata::encode::decode_pv_field_with_bitset;

        // NT-style wrapper with several non-`value` siblings.
        let desc = FieldDesc::Structure {
            struct_id: "epics:nt/NTScalar:1.0".into(),
            fields: vec![
                ("value".into(), FieldDesc::Scalar(ScalarType::Double)),
                (
                    "alarm".into(),
                    FieldDesc::Structure {
                        struct_id: "alarm_t".into(),
                        fields: vec![
                            ("severity".into(), FieldDesc::Scalar(ScalarType::Int)),
                            ("status".into(), FieldDesc::Scalar(ScalarType::Int)),
                            ("message".into(), FieldDesc::Scalar(ScalarType::String)),
                        ],
                    },
                ),
                (
                    "timeStamp".into(),
                    FieldDesc::Structure {
                        struct_id: "time_t".into(),
                        fields: vec![
                            (
                                "secondsPastEpoch".into(),
                                FieldDesc::Scalar(ScalarType::Long),
                            ),
                            ("nanoseconds".into(), FieldDesc::Scalar(ScalarType::Int)),
                        ],
                    },
                ),
            ],
        };
        let value = build_put_value(&desc, "42.5").unwrap();

        // Same BitSet the PUT ops build: only the `value` bit.
        let mut changed = BitSet::new();
        let value_bit = desc.bit_for_path("value").expect("value bit");
        changed.set(value_bit);

        for order in [ByteOrder::Little, ByteOrder::Big] {
            // Delta encode — what op_put_inner / op_put_value now emit.
            let mut delta = Vec::new();
            encode_pv_field_with_bitset(&value, &desc, &changed, 0, order, &mut delta);

            // Full encode — the previous (buggy) emission.
            let mut full = Vec::new();
            encode_pv_field(&value, &desc, order, &mut full);

            // The delta must be strictly smaller — siblings are omitted.
            assert!(
                delta.len() < full.len(),
                "delta ({}) must omit unmarked fields vs full ({}), order={order:?}",
                delta.len(),
                full.len()
            );

            // Decoding the delta with the SAME BitSet must consume every
            // byte and reproduce the marked field — this is exactly the
            // pvxs `from_wire_valid` contract.
            let mut cur = Cursor::new(delta.as_slice());
            let decoded = decode_pv_field_with_bitset(&desc, &changed, 0, &mut cur, order)
                .unwrap_or_else(|e| panic!("delta decode failed ({order:?}): {e:?}"));
            assert_eq!(
                cur.position() as usize,
                delta.len(),
                "BitSet-driven decode left trailing bytes, order={order:?}"
            );
            match &decoded {
                PvField::Structure(s) => {
                    assert_eq!(
                        s.get_field("value"),
                        Some(&PvField::Scalar(ScalarValue::Double(42.5))),
                        "value field mismatch, order={order:?}"
                    );
                }
                other => panic!("expected structure, got {other:?}"),
            }
        }
    }

    // ---- raw monitor control-frame classification ----------
    //
    // The raw loop previously had two swallow bugs: a frame shorter than
    // `ioid + subcmd` was skipped (`continue`), and a FINISH whose
    // required Status failed to decode fell through to a clean `Ok(())`
    // end-of-stream. `classify_raw_monitor_frame` is the single owner of
    // that policy; both malformed cases must be `Fatal`.

    #[test]
    fn bfr11_too_short_frame_is_fatal_decode() {
        // < 5 bytes: no room for ioid (4) + subcmd (1).
        match classify_raw_monitor_frame(&[0, 0, 0], ByteOrder::Little) {
            RawMonitorFrameKind::Fatal(PvaError::Decode(msg)) => {
                assert!(msg.contains("too short"), "msg: {msg}");
            }
            other => panic!("too-short frame must be Fatal(Decode), got {other:?}"),
        }
    }

    #[test]
    fn bfr11_finish_truncated_status_is_fatal_decode() {
        // subcmd 0x10 (FINISH) but NO status bytes after it → the
        // required Status cannot decode.
        let payload = [0u8, 0, 0, 0, 0x10];
        match classify_raw_monitor_frame(&payload, ByteOrder::Little) {
            RawMonitorFrameKind::Fatal(PvaError::Decode(msg)) => {
                assert!(msg.contains("FINISH status"), "msg: {msg}");
            }
            other => panic!("truncated FINISH status must be Fatal(Decode), got {other:?}"),
        }
    }

    #[test]
    fn bfr11_finish_success_status_is_clean_end() {
        let mut payload = vec![0u8, 0, 0, 0, 0x10];
        crate::proto::Status::ok().write_into(ByteOrder::Little, &mut payload);
        assert!(matches!(
            classify_raw_monitor_frame(&payload, ByteOrder::Little),
            RawMonitorFrameKind::FinishOk
        ));
    }

    #[test]
    fn bfr11_finish_error_status_is_fatal_protocol() {
        let mut payload = vec![0u8, 0, 0, 0, 0x10];
        crate::proto::Status::error("boom".to_string()).write_into(ByteOrder::Little, &mut payload);
        match classify_raw_monitor_frame(&payload, ByteOrder::Little) {
            RawMonitorFrameKind::Fatal(PvaError::Protocol(msg)) => {
                assert!(msg.contains("non-success"), "msg: {msg}");
            }
            other => panic!("non-success FINISH must be Fatal(Protocol), got {other:?}"),
        }
    }

    #[test]
    fn bfr11_data_frame_is_data() {
        // subcmd 0x00 = DATA, with a body trailing the header.
        let payload = [0u8, 0, 0, 0, 0x00, 1, 2, 3];
        assert!(matches!(
            classify_raw_monitor_frame(&payload, ByteOrder::Little),
            RawMonitorFrameKind::Data
        ));
    }

    #[test]
    fn ack_threshold_is_half_window_clamped_to_one() {
        // pvxs default ackAt = queueSize/2, clamped to [1, queueSize].
        assert_eq!(ack_threshold(1), 1); // 1/2 = 0 -> max(1, 0)
        assert_eq!(ack_threshold(2), 1);
        assert_eq!(ack_threshold(4), 2); // the DEFAULT_PIPELINE_SIZE case
        assert_eq!(ack_threshold(8), 4);
        assert_eq!(ack_threshold(33), 16);
    }

    fn opts(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    /// PVA-RS-2026-05-28-45: a custom monitor pvRequest's pipeline
    /// options must drive the wire pipeline bit / `nack` trailer and ACK
    /// cadence — not the client's fixed builder window. Boundary cases
    /// from the finding, plus the pvxs `clientmon.cpp:761-808` defaults.
    #[test]
    fn monitor_flow_from_record_options_matches_pvxs() {
        // pipeline=true,queueSize=16,ackAny=75% → window 16, ackAt 12.
        let f = MonitorFlow::from_record_options(
            &opts(&[("pipeline", "true"), ("queueSize", "16"), ("ackAny", "75%")]),
            DEFAULT_PIPELINE_SIZE,
        );
        assert!(f.pipeline);
        assert_eq!(f.queue_size, 16);
        assert_eq!(f.ack_at, 12); // 0.75 * 16

        // pipeline=true,queueSize=2 → window 2, ackAt = 2/2 = 1.
        let f = MonitorFlow::from_record_options(
            &opts(&[("pipeline", "true"), ("queueSize", "2")]),
            DEFAULT_PIPELINE_SIZE,
        );
        assert!(f.pipeline);
        assert_eq!(f.queue_size, 2);
        assert_eq!(f.ack_at, 1);

        // pipeline=false → no window, no ACK, no trailer regardless of
        // the builder default being nonzero.
        let f = MonitorFlow::from_record_options(&opts(&[("pipeline", "false")]), 4);
        assert!(!f.pipeline);
        assert_eq!(f.queue_size, 0);
        assert_eq!(f.ack_at, 0);

        // No pipeline option at all → pvxs default false.
        let f = MonitorFlow::from_record_options(&opts(&[("queueSize", "16")]), 4);
        assert!(!f.pipeline);
        assert_eq!(f.queue_size, 0);

        // pipeline=true, no queueSize → builder default window.
        let f = MonitorFlow::from_record_options(&opts(&[("pipeline", "true")]), 8);
        assert!(f.pipeline);
        assert_eq!(f.queue_size, 8);
        assert_eq!(f.ack_at, 4);

        // pipeline=true, invalid queueSize=1 (< 2) → builder default.
        let f = MonitorFlow::from_record_options(
            &opts(&[("pipeline", "true"), ("queueSize", "1")]),
            DEFAULT_PIPELINE_SIZE,
        );
        assert!(f.pipeline);
        assert_eq!(f.queue_size, DEFAULT_PIPELINE_SIZE);

        // ackAny as an absolute count is honored and clamped to window.
        let f = MonitorFlow::from_record_options(
            &opts(&[("pipeline", "true"), ("queueSize", "8"), ("ackAny", "2")]),
            DEFAULT_PIPELINE_SIZE,
        );
        assert_eq!(f.ack_at, 2);
        let f = MonitorFlow::from_record_options(
            &opts(&[("pipeline", "true"), ("queueSize", "4"), ("ackAny", "99")]),
            DEFAULT_PIPELINE_SIZE,
        );
        assert_eq!(f.ack_at, 4); // clamp to queue_size
    }

    /// The default (no-custom-request) path is unchanged: the builder
    /// window is the single source of truth, matching the pre-fix
    /// `pipeline_size`-driven behavior exactly.
    #[test]
    fn monitor_flow_window_preserves_default_behavior() {
        let f = MonitorFlow::window(0);
        assert!(!f.pipeline);

        let f = MonitorFlow::window(4);
        assert!(f.pipeline);
        assert_eq!(f.queue_size, 4);
        assert_eq!(f.ack_at, ack_threshold(4));
    }

    fn idle_sub_state() -> Arc<SubscriptionState> {
        Arc::new(SubscriptionState {
            active: parking_lot::Mutex::new(None),
            paused: std::sync::atomic::AtomicBool::new(false),
            stop: std::sync::atomic::AtomicBool::new(false),
            stats: parking_lot::Mutex::new(SubscriptionStat::default()),
            cancel: tokio::sync::Notify::new(),
        })
    }

    /// Regression for PVA-RS-2026-05-28-46: a monitor loop parked in
    /// `stream.recv().await` waits on `cancel` via `select!`. The single
    /// teardown owner must wake it. Model the loop's wait with a task
    /// that only awaits `cancel.notified()`; `teardown()` must make it
    /// return promptly instead of hanging forever.
    #[tokio::test]
    async fn teardown_wakes_a_parked_monitor_loop() {
        let state = idle_sub_state();
        let task_state = state.clone();
        let task = tokio::spawn(async move {
            task_state.cancel.notified().await;
        });
        // Give the task a chance to reach `.notified().await`.
        tokio::task::yield_now().await;
        state.teardown();
        tokio::time::timeout(std::time::Duration::from_secs(1), task)
            .await
            .expect("parked loop must wake on teardown, not hang")
            .expect("loop task panicked");
        assert!(
            state.stop.load(std::sync::atomic::Ordering::Relaxed),
            "teardown must set the terminal stop flag"
        );
    }

    /// `Notify` stores a single permit, so a cancel that races ahead of
    /// the loop reaching its `select!` is not lost: a later `notified()`
    /// completes immediately. Without this guarantee `stop_sync()` issued
    /// on a not-yet-parked loop could still hang.
    #[tokio::test]
    async fn teardown_before_park_is_not_lost() {
        let state = idle_sub_state();
        state.teardown(); // notify_one() with no waiter -> stored permit.
        tokio::time::timeout(std::time::Duration::from_secs(1), state.cancel.notified())
            .await
            .expect("stored cancel permit must satisfy a later notified()");
    }

    /// `teardown()` is the shared owner for `stop()`, `stop_sync()`, and
    /// `Drop`; calling it more than once (e.g. `stop_sync` then `Drop`)
    /// must be a harmless no-op.
    #[tokio::test]
    async fn teardown_is_idempotent() {
        let state = idle_sub_state();
        state.teardown();
        state.teardown();
        assert!(state.stop.load(std::sync::atomic::Ordering::Relaxed));
        assert!(state.active.lock().is_none());
    }

    // ── op-response decode-fault → circuit close regressions ─────────────
    //
    // pvxs resets the circuit (`bev.reset()`, clientget.cpp:456-493) on a
    // malformed op-response body or a command/subcommand mismatch, but
    // delivers a non-success Status to the operation without resetting.
    // The PUT_GET/PROCESS decoders carry that distinction structurally: an
    // outer `Err` is connection-fatal (the caller closes the `ServerConn`);
    // a non-success Status is a per-op result, NOT an `Err`. These tests
    // lock that split so a later edit cannot fold the two meanings back
    // together (which is what left the circuit reusable after a bad frame).

    fn scalar_int_struct() -> FieldDesc {
        FieldDesc::Structure {
            struct_id: "epics:nt/NTScalar:1.0".into(),
            fields: vec![("value".into(), FieldDesc::Scalar(ScalarType::Int))],
        }
    }

    /// PUT_GET INIT: `ioid + subcmd + status [+ putIF + getIF]`.
    fn put_get_init_frame(
        order: ByteOrder,
        status: crate::proto::Status,
        with_types: bool,
    ) -> Frame {
        let mut payload = Vec::new();
        payload.put_u32(7, order);
        payload.put_u8(QosFlags::INIT);
        status.write_into(order, &mut payload);
        if with_types {
            let desc = scalar_int_struct();
            encode_type_desc(&desc, order, &mut payload); // putIF
            encode_type_desc(&desc, order, &mut payload); // getIF
        }
        let header =
            PvaHeader::application(true, order, Command::PutGet.code(), payload.len() as u32);
        Frame { header, payload }
    }

    #[test]
    fn put_get_init_non_success_status_is_per_op_not_fatal() {
        let frame = put_get_init_frame(
            ByteOrder::Little,
            crate::proto::Status::error("denied"),
            false,
        );
        let mut cache = crate::pvdata::encode::TypeCache::new();
        match decode_put_get_init(&frame, &mut cache) {
            Ok(Err(s)) => assert!(!s.is_success(), "expected the carried non-success status"),
            other => panic!("non-success INIT must be per-op Ok(Err(status)), got {other:?}"),
        }
    }

    #[test]
    fn put_get_init_success_decodes_get_introspection() {
        let frame = put_get_init_frame(ByteOrder::Little, crate::proto::Status::ok(), true);
        let mut cache = crate::pvdata::encode::TypeCache::new();
        match decode_put_get_init(&frame, &mut cache) {
            Ok(Ok(intro)) => assert!(matches!(intro, FieldDesc::Structure { .. })),
            other => panic!("successful INIT must yield Ok(Ok(getIF)), got {other:?}"),
        }
    }

    #[test]
    fn put_get_init_truncated_status_is_fatal_err() {
        // ioid + subcmd only — the Status decode runs off the end.
        let mut frame = put_get_init_frame(ByteOrder::Little, crate::proto::Status::ok(), false);
        frame.payload.truncate(5); // u32 ioid + u8 subcmd, no status
        frame.header.payload_length = frame.payload.len() as u32;
        let mut cache = crate::pvdata::encode::TypeCache::new();
        assert!(
            decode_put_get_init(&frame, &mut cache).is_err(),
            "a truncated INIT body must be a connection-fatal Err, not Ok"
        );
    }

    #[test]
    fn put_get_init_wrong_command_is_fatal_err() {
        let mut frame = put_get_init_frame(ByteOrder::Little, crate::proto::Status::ok(), true);
        frame.header.command = Command::Get.code(); // command mismatch
        let mut cache = crate::pvdata::encode::TypeCache::new();
        assert!(
            decode_put_get_init(&frame, &mut cache).is_err(),
            "a command mismatch must be a connection-fatal Err"
        );
    }

    /// PUT_GET data: `ioid + subcmd + status [+ get bitset + get value]`.
    fn put_get_data_frame(
        order: ByteOrder,
        status: crate::proto::Status,
        with_value: bool,
    ) -> Frame {
        let mut payload = Vec::new();
        payload.put_u32(7, order);
        payload.put_u8(0x00);
        status.write_into(order, &mut payload);
        if with_value {
            let desc = scalar_int_struct();
            let mut changed = BitSet::new();
            changed.set(desc.bit_for_path("value").unwrap());
            changed.write_into(order, &mut payload);
            let value = build_put_value(&desc, "11").unwrap();
            encode_pv_field_with_bitset(&value, &desc, &changed, 0, order, &mut payload);
        }
        let header =
            PvaHeader::application(true, order, Command::PutGet.code(), payload.len() as u32);
        Frame { header, payload }
    }

    #[test]
    fn put_get_data_non_success_status_is_per_op_not_fatal() {
        let frame = put_get_data_frame(
            ByteOrder::Little,
            crate::proto::Status::error("nope"),
            false,
        );
        let desc = scalar_int_struct();
        let mut cache = crate::pvdata::encode::TypeCache::new();
        match decode_put_get_data(&frame, &desc, &mut cache) {
            Ok(Err(s)) => assert!(!s.is_success()),
            other => panic!("non-success data must be per-op Ok(Err(status)), got {other:?}"),
        }
    }

    #[test]
    fn put_get_data_truncated_value_is_fatal_err() {
        // ok status but the bitset/value never arrive.
        let frame = put_get_data_frame(ByteOrder::Little, crate::proto::Status::ok(), false);
        let desc = scalar_int_struct();
        let mut cache = crate::pvdata::encode::TypeCache::new();
        assert!(
            decode_put_get_data(&frame, &desc, &mut cache).is_err(),
            "a truncated successful data body must be a connection-fatal Err"
        );
    }

    /// PROCESS response: `ioid + subcmd + status`.
    fn process_frame(order: ByteOrder, status: crate::proto::Status, full: bool) -> Frame {
        let mut payload = Vec::new();
        payload.put_u32(7, order);
        payload.put_u8(0x00);
        if full {
            status.write_into(order, &mut payload);
        }
        let header =
            PvaHeader::application(true, order, Command::Process.code(), payload.len() as u32);
        Frame { header, payload }
    }

    #[test]
    fn process_non_success_status_is_per_op_not_fatal() {
        let frame = process_frame(ByteOrder::Little, crate::proto::Status::error("busy"), true);
        match decode_process_status(&frame) {
            Ok(s) => assert!(
                !s.is_success(),
                "non-success PROCESS status is data, not Err"
            ),
            Err(e) => panic!("non-success PROCESS must decode to Ok(status), got Err({e:?})"),
        }
    }

    #[test]
    fn process_truncated_is_fatal_err() {
        let frame = process_frame(ByteOrder::Little, crate::proto::Status::ok(), false);
        assert!(
            decode_process_status(&frame).is_err(),
            "a truncated PROCESS body must be a connection-fatal Err"
        );
    }

    #[test]
    fn process_wrong_command_is_fatal_err() {
        let mut frame = process_frame(ByteOrder::Little, crate::proto::Status::ok(), true);
        frame.header.command = Command::Get.code();
        assert!(
            decode_process_status(&frame).is_err(),
            "a command mismatch must be a connection-fatal Err"
        );
    }
}
