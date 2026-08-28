//! Channel-aware ops with automatic reconnect.
//!
//! These replace the older `ops::*` functions which operated on a one-shot
//! `Connection` with no reconnect logic. The v2 versions take a
//! [`Channel`] and:
//!
//! - GET / PUT / RPC: re-queued and re-issued when the channel is lost
//!   mid-op, bounded by the caller's `op_timeout` — see
//!   `requeue_on_disconnect`. pvxs does NOT surface the loss to the
//!   caller: `GPROp::disconnected` (`clientget.cpp:380-404`) returns the
//!   op to `chan->pending` for every one-call (`autoExec`) op, GET and PUT
//!   alike.
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

use epics_base_rs::runtime::task::timeout;
use tokio::sync::mpsc;
use tracing::debug;

use crate::codec::PvaCodec;
use crate::error::{PvaError, PvaResult};
use crate::proto::{BitSet, ByteOrder, Command, PvaHeader, QosFlags, ReadExt, WriteExt};
use crate::pv_request::{build_pv_request_fields, build_pv_request_value_only};
use crate::pvdata::encode::{
    default_value_for, encode_pv_field, encode_pv_field_with_bitset, encode_type_desc,
};
use crate::pvdata::{
    FieldDesc, PvField, PvStructure, RpcReply, ScalarType, ScalarValue, UnionItem, VariantValue,
};

use super::channel::Channel;
use super::decode::{
    Frame, GetFieldResponse, OpResponse, decode_get_field_response, decode_op_response,
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
/// The frame is decoded with an EMPTY type cache: the connection reader
/// task has already flattened every 0xFD/0xFE marker — in both the INIT
/// descriptor region and inside `any` DATA values — against its single
/// reader-owned cache, so a routed frame is self-contained
/// ([`flatten_type_cache_markers`](super::decode::flatten_type_cache_markers)).
/// pvxs decodes both through the one connection `rxRegistry`
/// (`clientget.cpp:410-451`, `dataencode.cpp:542`); folding the value
/// markers into the same reader-owned cache is the parity equivalent. Per-op
/// tasks therefore never share decode-time cache state, which makes the
/// cross-op decode-order race structurally impossible.
fn decode_op_or_reset(
    server: &ServerConn,
    frame: &Frame,
    introspection: Option<&FieldDesc>,
) -> PvaResult<OpResponse> {
    decode_op_response(frame, introspection).inspect_err(|_| server.close())
}

/// GET_FIELD analog: pvxs resets the circuit on a bad GET_FIELD descriptor
/// buffer (`clientintrospect.cpp:115-133`). A non-success `Status` decodes
/// to `Ok` (no introspection) and stays per-op at the caller.
fn decode_get_field_or_reset(server: &ServerConn, frame: &Frame) -> PvaResult<GetFieldResponse> {
    decode_get_field_response(frame).inspect_err(|_| server.close())
}

// pvxs seeds each ID namespace from a distinct non-zero base (commit
// 3b641bed). IOID base = pvxs `clientimpl.h:106` `nextIOID=0x10002000`.
static NEXT_IOID: AtomicU32 = AtomicU32::new(0x1000_2000);
fn alloc_ioid() -> u32 {
    NEXT_IOID.fetch_add(1, Ordering::Relaxed)
}

/// Default credit window (`queueSize`) used **once a monitor is
/// pipelined** but the pvRequest names no `queueSize` — pvxs
/// `MonitorBuilder`'s `queueSize=4` default (`clientmon.cpp:50`). This is
/// the fallback queue depth, NOT a default that turns pipelining on: the
/// default monitor is non-pipelined (`PvaClientBuilder::pipeline_size`
/// defaults to 0, matching pvxs `pipeline=false`).
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
    /// pvxs `op->queueSize` (`clientmon.cpp:52`, default 4) — the NEGOTIATED
    /// monitor queue depth. Reported as `SubscriptionStat::limit_queue` (pvxs
    /// `ret.limitQueue = queueSize`, `clientmon.cpp:152`) and, when `pipeline`,
    /// written as the INIT `nack` trailer.
    ///
    /// This is resolved whether or not pipelining is on, exactly as pvxs
    /// resolves it — the `queueSize` block sits outside any `if(pipeline)`
    /// (`clientmon.cpp:761-773`). It used to collapse to `0` for a
    /// non-pipelined monitor, which gave the field two meanings ("no pipeline"
    /// vs. a real depth) and made a `record[queueSize=16]` monitor report
    /// `limit_queue = 0` (R10-35). `pipeline` alone gates the wire.
    pub queue_size: u32,
    /// pvxs `op->ackAt` (`clientmon.cpp:796-808`) — refill the server's window
    /// after this many delivered events. Resolved unconditionally, as pvxs
    /// does; consumed only when `pipeline`.
    pub ack_at: u32,
}

impl MonitorFlow {
    /// Default-path flow control: the client's configured pipeline
    /// window is the single source of truth (no caller pvRequest to
    /// honor). `pipeline_size == 0` disables pipelining entirely,
    /// matching the pre-`MonitorFlow` `pipeline_size > 0` gate.
    ///
    /// A non-pipelined monitor still has a queue depth — pvxs's builder
    /// default `queueSize = 4` stands whatever `pipeline` is
    /// (`clientmon.cpp:52`), and it is what `stats()` reports. So the
    /// `pipeline_size == 0` arm keeps [`DEFAULT_PIPELINE_SIZE`] as the depth
    /// rather than reporting `0`.
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
                queue_size: DEFAULT_PIPELINE_SIZE,
                ack_at: ack_threshold(DEFAULT_PIPELINE_SIZE),
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
    ///
    /// Every option is read through the CONVERSION owner
    /// ([`crate::pvdata::convert`], the port's `Value::copyOut`), because that
    /// is what pvxs's `Value::as<T>()` is. This used to normalise each option
    /// to its DISPLAY STRING and then string-match, which diverged three ways
    /// (R10-35):
    ///
    /// * `pipeline` matched the texts `"true"`/`"1"`/`"yes"`. pvxs runs
    ///   `as(bool)`: bool, ANY non-zero integer or real is true
    ///   (`data.cpp:405`), while a STRING converts only as the exact tokens
    ///   `"true"`/`"false"` (`data.cpp:466-469`). So `pipeline = Int(2)` ran a
    ///   pipelined monitor against pvxs and a plain one here, and `"yes"` did
    ///   the reverse.
    /// * `queueSize` was parsed as a DECIMAL string. pvxs runs `as(uint32)`,
    ///   which casts a real and parses a string with `parseTo<uint64_t>` =
    ///   `stoull(s, &idx, 0)` — BASE 0. `"0x10"` is 16 and `Double(8.5)` is 8.
    /// * `ackAny` took the percent branch off the RENDERED text. pvxs gates it
    ///   on `ackAny.type()==TypeCode::String` (`clientmon.cpp:777`) and only
    ///   then reads the `"N%"` form.
    ///
    /// Note this is the CLIENT's conversion topology, which is NOT the
    /// server's: `servermon.cpp:556` runs the THROWING `ackAny.as<std::string>()`
    /// ahead of both branches, so a non-convertible `ackAny` resets the circuit
    /// there. The client's `type()==String` guard means nothing here can throw.
    pub fn from_record_options(
        record_options: &[(String, crate::pvdata::ScalarValue)],
        default_window: u32,
    ) -> Self {
        use crate::pvdata::{PvField, convert};
        // The extractor only lifts SCALAR leaves out of `record._options`, so a
        // non-scalar option arrives as absent. That matches pvxs's outcome for
        // one: every `as<T>()` it would run on an array / struct is `NoConvert`,
        // and `as(x)` answers false, leaving the default in place.
        let get = |key: &str| -> Option<PvField> {
            record_options
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| PvField::Scalar(v.clone()))
        };

        // `clientmon.cpp:761-773` — `if(queueSize.as(Q) && Q>1) op->queueSize = Q;`
        // else keep the default. Resolved OUTSIDE any pipeline gate, exactly
        // where pvxs resolves it. A pipeline window must be >= 2, so a 0/1
        // configured default falls back to pvxs's 4.
        let fallback = if default_window > 1 {
            default_window
        } else {
            DEFAULT_PIPELINE_SIZE
        };
        let queue_size = get("queueSize")
            .and_then(|f| convert::as_u32(&f).ok())
            .filter(|&q| q > 1)
            .unwrap_or(fallback);

        // `clientmon.cpp:775` — `(void)options["pipeline"].as(op->pipeline);`
        // An absent or unconvertible value leaves the `false` default.
        let pipeline = get("pipeline")
            .and_then(|f| convert::as_bool(&f).ok())
            .unwrap_or(false);

        // `clientmon.cpp:777-808`. Resolved unconditionally, as pvxs does;
        // only a pipelined monitor ever consumes it.
        let ack_any = record_options
            .iter()
            .find(|(k, _)| k == "ackAny")
            .map(|(_, v)| v.clone());
        let ack_at = ack_at_from_request(ack_any.as_ref(), queue_size);

        Self {
            pipeline,
            queue_size,
            ack_at,
        }
    }
}

/// pvxs `clientmon.cpp:777-808` — derive the ACK-refill threshold `ackAt`
/// from `record._options.ackAny` and the negotiated `queue_size`.
///
/// pvxs's order, which this mirrors exactly:
///
/// 1. `if(ackAny.type()==TypeCode::String)` — ONLY a string-STORED value can
///    take the percent branch, and only in the `"N%"` shape (`size()>1` and a
///    trailing `%`). The percent must land in `(0, 100]`, else pvxs throws to
///    its own `catch` and leaves `ackAt` at 0. Note the branch is chosen by
///    STORAGE, not by how the value happens to render — the port used to strip
///    a `%` off the displayed text of any value.
/// 2. `if(ackAt==0) { if(ackAny.as(count)) ackAt = count; }` — the integer
///    conversion, run for EVERY storage including string. That is `as(uint32)`,
///    so a string parses BASE 0 (`"0x10"` → 16), a real casts (`Double(3.7)` →
///    3), and a bool converts (`true` → 1). The port used a decimal-only
///    `str::parse::<u32>`, which refused all three.
/// 3. `if(ackAt==0) ackAt = queueSize/2;` then clamp to `[1, queueSize]`.
///
/// The distinction between the string that FAILS the percent parse and one
/// that never had a `%` is invisible in the result: both fall through to step
/// 2, which re-reads the WHOLE string as an integer (`"50%"` fails that too),
/// then to the half-window default.
///
/// `queue_size` is always `>= 2` here (the caller's `Q > 1` filter and its
/// `>= 2` fallback), so the clamp cannot invert.
fn ack_at_from_request(ack_any: Option<&crate::pvdata::ScalarValue>, queue_size: u32) -> u32 {
    use crate::pvdata::{PvField, ScalarValue, convert};

    let mut ack_at: u32 = 0;
    if let Some(v) = ack_any {
        // Step 1 — `ackAny.type()==TypeCode::String`, the STORAGE test.
        if let ScalarValue::String(s) = v {
            let s = s.as_str_lossy();
            if s.len() > 1
                && let Some(pct) = s.strip_suffix('%')
                && let Ok(percent) = pct.trim().parse::<f64>()
                && percent > 0.0
                && percent <= 100.0
            {
                ack_at = (percent / 100.0 * queue_size as f64) as u32;
            }
        }
        // Step 2 — `ackAny.as(count)`, the conversion owner, every storage.
        if ack_at == 0
            && let Ok(count) = convert::as_u32(&PvField::Scalar(v.clone()))
        {
            ack_at = count;
        }
    }
    // Step 3.
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
                big_endian: matches!(self.server.byte_order(), ByteOrder::Big),
            };
            let frame = codec.build_destroy_request(sid, self.ioid);
            let _ = self.server.try_send(frame);
        }
        self.server.unregister_ioid(self.ioid);
    }
}

// ── GET ────────────────────────────────────────────────────────────────

/// A GET / PUT_GET reply plus the leaves the SERVER marked in it.
///
/// A PVA data reply is `changed | value`: the server frames only the
/// leaves it assigned (`to_wire_valid`, `serverget.cpp:104`), and the
/// decoder zero-fills the rest. The zero-fill is invisible in the decoded
/// [`PvField`], so a consumer that must re-publish the reply — a gateway
/// forwarding an upstream read downstream — has to be told which leaves
/// were real. `marked` is that fact, in the same dot-path form the server
/// side uses ([`crate::server_native::source::SourceRead`]); dropping it
/// and re-framing a full mask would put fabricated zeros on the wire for
/// leaves the upstream never sent.
#[derive(Debug, Clone)]
pub struct MarkedRead {
    /// Reply introspection (the GET-side descriptor).
    pub desc: FieldDesc,
    /// The decoded value.
    pub value: PvField,
    /// Field paths the reply's changed-bitset marked, or `None` when the
    /// root bit (0) was set — the wire's way of saying "the whole
    /// structure", which needs no per-leaf list.
    pub marked: Option<Vec<String>>,
}

/// The changed-leaf paths of a reply bitset. Root bit ⇒ `None` (whole
/// structure), matching the gateway's monitor-cache decode of the same
/// wire field.
fn reply_marks(desc: &FieldDesc, changed: &BitSet) -> Option<Vec<String>> {
    if changed.get(0) {
        None
    } else {
        Some(crate::pvdata::encode::changed_bitset_paths(desc, changed))
    }
}

pub async fn op_get(
    channel: &Arc<Channel>,
    fields: &[&str],
    op_timeout: Duration,
) -> PvaResult<(FieldDesc, PvField)> {
    op_get_inner(channel, fields, None, op_timeout)
        .await
        .map(|r| (r.desc, r.value))
}

/// [`op_get`] keeping the reply's marked leaves — for a caller that
/// re-frames the value with a changed-bitset of its own (the PVA gateway).
pub async fn op_get_marked(
    channel: &Arc<Channel>,
    fields: &[&str],
    op_timeout: Duration,
) -> PvaResult<MarkedRead> {
    op_get_inner(channel, fields, None, op_timeout).await
}

/// [`op_get_raw`] keeping the reply's marked leaves. See [`MarkedRead`].
pub async fn op_get_raw_marked(
    channel: &Arc<Channel>,
    pv_req: &[u8],
    op_timeout: Duration,
) -> PvaResult<MarkedRead> {
    op_get_inner(channel, &[], Some(pv_req), op_timeout).await
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
    op_get_inner(channel, &[], Some(pv_req), op_timeout)
        .await
        .map(|r| (r.desc, r.value))
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
    match epics_base_rs::runtime::task::timeout(op_timeout, channel.ensure_active()).await {
        Ok(result) => result,
        Err(_) => Err(PvaError::Timeout),
    }
}

/// The single re-queue owner: pvxs `OperationBase::disconnected`
/// (`clientimpl.h:62`), reached from `Channel::disconnect`
/// (`src/client.cpp:198-204`) on a circuit drop AND on a server-initiated
/// `CMD_DESTROY_CHANNEL`.
///
/// pvxs does not fail an in-flight one-shot when its channel goes away.
/// `GPROp::disconnected` (`clientget.cpp:380-404`) pushes the op back into
/// `chan->pending` and returns it to `Connecting`, the channel re-enters a
/// search bucket (`src/client.cpp:209-213`), and `Channel::createOperations`
/// (`src/client.cpp:120-146`) re-issues every pending op once the channel is
/// Active again; the caller's future stays pending across all of it. The
/// single exception is `state==Exec && op!=Get && !autoExec` ("can't
/// restart as server side-effects may occur"), which is the two-phase
/// application API — every op here is the one-call `autoExec=true` form
/// (`clientget.cpp:126`), so the rule is uniform and no op opts out. A PUT
/// is re-issued exactly as pvxs re-issues it.
///
/// `attempt` is re-run from the top, including its
/// [`ensure_active_with_op_timeout`] — that is what re-searches, since
/// `Channel::ensure_active` forces a fresh search once the server has
/// destroyed the SID. Only [`PvaError::Disconnected`] re-queues: a remote
/// `Status`, a decode fault, or an expired deadline is the op's answer.
/// `op_timeout` is the whole operation's budget, not one attempt's, so a
/// server that keeps dropping the channel still ends at the caller's
/// deadline rather than looping forever.
pub(crate) async fn requeue_on_disconnect<F, Fut, T>(
    channel: &Arc<Channel>,
    op_timeout: Duration,
    attempt: F,
) -> PvaResult<T>
where
    F: Fn(Duration) -> Fut,
    Fut: std::future::Future<Output = PvaResult<T>>,
{
    let deadline = std::time::Instant::now() + op_timeout;
    loop {
        let budget = deadline.saturating_duration_since(std::time::Instant::now());
        if budget.is_zero() {
            return Err(PvaError::Timeout);
        }
        match attempt(budget).await {
            Err(PvaError::Disconnected) => {
                debug!(
                    pv = %channel.pv_name,
                    "channel lost mid-op — re-queueing (pvxs GPROp::disconnected)"
                );
            }
            other => return other,
        }
    }
}

async fn op_get_inner(
    channel: &Arc<Channel>,
    fields: &[&str],
    raw_pv_req: Option<&[u8]>,
    op_timeout: Duration,
) -> PvaResult<MarkedRead> {
    requeue_on_disconnect(channel, op_timeout, |budget| {
        op_get_inner_attempt(channel, fields, raw_pv_req, budget)
    })
    .await
}

/// One attempt of [`op_get_inner`]; [`requeue_on_disconnect`] runs it
/// again from the top when the channel is lost mid-op.
async fn op_get_inner_attempt(
    channel: &Arc<Channel>,
    fields: &[&str],
    raw_pv_req: Option<&[u8]>,
    op_timeout: Duration,
) -> PvaResult<MarkedRead> {
    let (server, sid) = ensure_active_with_op_timeout(channel, op_timeout).await?;
    let order = server.byte_order();
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
                Ok(Some((intro, value, changed))) => {
                    // Re-cache so the next call also takes the fast path.
                    *channel.cached_get.lock() = Some(warm);
                    let marked = reply_marks(&intro, &changed);
                    return Ok(MarkedRead {
                        desc: (*intro).clone(),
                        value,
                        marked,
                    });
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
                    let order = server.byte_order();
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

    // INIT alone. The EXEC goes out only after the INIT reply lands (below)
    // — never pipelined into the same write.
    //
    // A pvxs server dispatches every complete message in the receive buffer in
    // ONE pass (`ConnBase::bevRead`, `while(bev && remaining >= 8)`,
    // conn.cpp:152-153), so two frames in one TCP segment are handled
    // back-to-back with nothing running between them. If the op's Source has
    // not connected inline — a gateway, an un-`open()`ed `SharedPV`
    // (sharedpv.cpp:243) — the op is still `ServerOp::Creating` when the EXEC
    // is dispatched, and `ServerConn::handle_GPR`'s EXEC branch answers that
    // with `bev.reset()` (serverget.cpp:429-434): the whole TCP circuit dies,
    // taking every channel on it, with no MESSAGE and no Status. Sending the
    // EXEC after the INIT reply makes the race unconstructible — the reply IS
    // the proof the op left `Creating`. MONITOR already does this (its START
    // is sent after the INIT reply); GET was the only pipelined op.
    //
    // Sync send into the unbounded writer queue — no scheduler hop,
    // mirrors CA's `DirectServerWriter::send_frame`.
    server.send_for_channel_sync(sid, codec.build_get_init(sid, ioid, &pv_req))?;

    // Receive INIT response
    let init_frame = await_oneshot_frame(rx_init, op_timeout).await?;
    let init = match decode_op_or_reset(&server, &init_frame, None)? {
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
        return Err(PvaError::RemoteError(init.status));
    }
    ioid_guard.arm_destroy(sid);
    let intro = init.introspection;

    // The INIT reply proves the server left `ServerOp::Creating`; only now is
    // the EXEC legal (see the INIT send above).
    server.send_for_channel_sync(sid, codec.build_get(sid, ioid))?;

    // Receive DATA response
    let data_frame = await_oneshot_frame(rx_data, op_timeout).await?;
    let intro_arc = Arc::new(intro);
    let result = match decode_op_or_reset(&server, &data_frame, Some(&intro_arc))? {
        OpResponse::Data(d) => {
            if d.status.is_success() {
                let marked = reply_marks(&intro_arc, &d.changed);
                Ok(MarkedRead {
                    desc: (*intro_arc).clone(),
                    value: d.value,
                    marked,
                })
            } else {
                Err(PvaError::RemoteError(d.status))
            }
        }
        // a data-phase failure now arrives as a status-only
        // reply (server echoes the request data subcmd, no bitset/value),
        // so it decodes to OpResponse::Status. Surface the server status
        // instead of mislabelling it "expected GET data, got Status".
        OpResponse::Status(s) => Err(PvaError::RemoteError(s.status)),
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
) -> PvaResult<Option<(Arc<FieldDesc>, PvField, BitSet)>> {
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
                Ok(Some((warm.intro.clone(), d.value, d.changed)))
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
    requeue_on_disconnect(channel, op_timeout, |budget| {
        op_get_field_attempt(channel, subfield, budget)
    })
    .await
}

/// One attempt of [`op_get_field`]; [`requeue_on_disconnect`] runs it
/// again from the top when the channel is lost mid-op.
async fn op_get_field_attempt(
    channel: &Arc<Channel>,
    subfield: &str,
    op_timeout: Duration,
) -> PvaResult<FieldDesc> {
    let (server, sid) = ensure_active_with_op_timeout(channel, op_timeout).await?;
    let order = server.byte_order();
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
        return Err(PvaError::RemoteError(resp.status));
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

/// PUT that writes no field: the DATA phase carries an EMPTY changed
/// bitset, so no value bytes reach the wire and the server applies only
/// the INIT pvRequest's `record._options`.
///
/// This is the interoperable spelling of "make the remote record
/// process". pvxs implements no CMD_PROCESS handler at all — the
/// constant exists once, in `src/pvaproto.h:632`, and `ConnBase`'s
/// command switch (`src/conn.cpp:249-276`) drops an unrecognised command
/// to `default:`, which debug-logs and `evbuffer_drain`s the body
/// without replying. pvxs's own pvalink forward link is this PUT:
/// `pvaScanForward` calls `lchan->put(true)` (`pvxs/ioc/pvalink_lset.cpp:683`)
/// and `linkBuildPut` returns the prototype untouched when no link
/// staged a value (`ioc/pvalink_channel.cpp:127-184`), under a pvRequest
/// carrying `record._options.process = "true"`
/// (`ioc/pvalink_channel.cpp:257-263`).
///
/// Use this, not [`op_process`], on any path whose peer may be a pvxs
/// server.
pub async fn op_put_empty(
    channel: &Arc<Channel>,
    pv_req: &[u8],
    op_timeout: Duration,
) -> PvaResult<()> {
    op_put_inner_build(
        channel,
        Some(pv_req),
        op_timeout,
        // Nothing is written, so no snapshot can inform the build.
        |_intro| false,
        // The empty bitset is the payload: `encode_pv_field_with_bitset`
        // emits no bytes for an unset bit, so the prototype-shaped value
        // only satisfies the builder signature and never reaches the wire.
        |intro, _previous| Ok((default_value_for(intro), BitSet::new())),
    )
    .await
}

/// PUT the legacy pvAccessCPP positional bare-token form, classified
/// against the PUT prototype once the server returns it. Mirrors
/// `pvtoolsSrc/pvput.cpp:144-178`: a scalar-array `.value` drops the
/// first token (the compatibility length, ignored) and writes the
/// rest; a lone `[...]` token is the JSON-array `value=[...]` shortcut;
/// a scalar `.value` takes exactly one token (more than one is an
/// error, matching upstream's "Can't assign multiple values to
/// scalar"). Token classification is deferred to `op_put_inner_build`
/// because the array-vs-scalar decision needs the prototype.
pub async fn op_put_tokens(
    channel: &Arc<Channel>,
    tokens: &[String],
    raw_pv_req: Option<&[u8]>,
    op_timeout: Duration,
) -> PvaResult<()> {
    op_put_inner_build(
        channel,
        raw_pv_req,
        op_timeout,
        value_target_is_enum,
        |intro, previous| build_put_value_mode(intro, tokens, previous),
    )
    .await
}

/// PUT the raw CLI value tokens, deferring every field-vs-bare
/// classification to the server PUT prototype. This is the single
/// prototype-aware classifier the `pvput-rs` CLI uses: a `field=value`
/// token is a field assignment only when `field` exists in the
/// prototype, otherwise it is a bare string value (when `.value` is a
/// string) or warned-and-ignored — exactly pvAccessCPP
/// `pvtoolsSrc/pvput.cpp:109-235`. No CLI-side guess is made before the
/// structure is known. When `raw_pv_req` is `None` the INIT pvRequest
/// selects all fields (pvAccessCPP's empty default request), so the
/// full writable prototype is available to classify against.
pub async fn op_put_args(
    channel: &Arc<Channel>,
    tokens: &[String],
    raw_pv_req: Option<&[u8]>,
    op_timeout: Duration,
) -> PvaResult<()> {
    // Default to the select-all sentinel (not value-only) so the server
    // returns every writable field for classification, matching
    // pvAccessCPP's empty default request (pvutils.cpp:31).
    let pv_req = raw_pv_req.unwrap_or_else(|| sentinel_all_fields());
    op_put_inner_build(
        channel,
        Some(pv_req),
        op_timeout,
        value_target_is_enum,
        |intro, previous| build_put_from_args(intro, tokens, previous),
    )
    .await
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
    requeue_on_disconnect(channel, op_timeout, |budget| {
        op_put_field_attempt(channel, field_path, value_str, budget)
    })
    .await
}

/// One attempt of [`op_put_field`]; [`requeue_on_disconnect`] runs it
/// again from the top when the channel is lost mid-op.
async fn op_put_field_attempt(
    channel: &Arc<Channel>,
    field_path: &str,
    value_str: &str,
    op_timeout: Duration,
) -> PvaResult<()> {
    let (server, sid) = ensure_active_with_op_timeout(channel, op_timeout).await?;
    let order = server.byte_order();
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

    let init_req = codec.build_put_init(sid, ioid, &pv_req);
    server.send_for_channel(sid, init_req).await?;
    let init_frame = await_frame(&mut stream, op_timeout).await?;
    let init = match decode_op_or_reset(&server, &init_frame, None)? {
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
        return Err(PvaError::RemoteError(init.status));
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
                Err(PvaError::RemoteError(s.status))
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
///
/// Text leaves only — each value is parsed against its target
/// descriptor. Use [`op_put_fields_typed`] to assign already-typed
/// pvData payloads (e.g. typed arrays staged by pvalink OUT links).
pub async fn op_put_fields(
    channel: &Arc<Channel>,
    assignments: &[(String, String)],
    pv_req_override: Option<&[u8]>,
    op_timeout: Duration,
) -> PvaResult<()> {
    let typed: Vec<(String, PutLeaf)> = assignments
        .iter()
        .map(|(p, v)| (p.clone(), PutLeaf::Str(v.clone())))
        .collect();
    op_put_fields_inner(channel, &typed, pv_req_override, op_timeout).await
}

/// PUT multiple field assignments as a single delta, with each leaf
/// either a parsed text value or an already-typed pvData payload (see
/// [`PutLeaf`]). The typed counterpart of [`op_put_fields`]: a typed
/// leaf is placed into the selected descriptor without serializing
/// through `Display`/`ScalarValue::parse`, so a scalar array keeps its
/// element type and byte content. pvxs `linkBuildPut` combined
/// sibling-field PUT parity (`pvxs/ioc/pvalink_channel.cpp:127-184`).
pub async fn op_put_fields_typed(
    channel: &Arc<Channel>,
    assignments: &[(String, PutLeaf)],
    pv_req_override: Option<&[u8]>,
    op_timeout: Duration,
) -> PvaResult<()> {
    op_put_fields_inner(channel, assignments, pv_req_override, op_timeout).await
}

/// Shared machinery for the multi-field PUT: INIT (with the caller's
/// `pv_req_override` or a request derived from the assigned paths),
/// build the delta from the prototype via the single owner
/// [`build_field_delta`], send, await completion. Both [`op_put_fields`]
/// and [`op_put_fields_typed`] funnel through here so the INIT/await/
/// destroy machinery has one owner and cannot drift between the text and
/// typed paths.
async fn op_put_fields_inner(
    channel: &Arc<Channel>,
    assignments: &[(String, PutLeaf)],
    pv_req_override: Option<&[u8]>,
    op_timeout: Duration,
) -> PvaResult<()> {
    requeue_on_disconnect(channel, op_timeout, |budget| {
        op_put_fields_inner_attempt(channel, assignments, pv_req_override, budget)
    })
    .await
}

/// One attempt of [`op_put_fields_inner`]; [`requeue_on_disconnect`] runs it
/// again from the top when the channel is lost mid-op.
async fn op_put_fields_inner_attempt(
    channel: &Arc<Channel>,
    assignments: &[(String, PutLeaf)],
    pv_req_override: Option<&[u8]>,
    op_timeout: Duration,
) -> PvaResult<()> {
    if assignments.is_empty() {
        return Err(PvaError::InvalidValue("no field assignments".into()));
    }
    let (server, sid) = ensure_active_with_op_timeout(channel, op_timeout).await?;
    let order = server.byte_order();
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

    let init_req = codec.build_put_init(sid, ioid, pv_req);
    server.send_for_channel(sid, init_req).await?;
    let init_frame = await_frame(&mut stream, op_timeout).await?;
    let init = match decode_op_or_reset(&server, &init_frame, None)? {
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
        return Err(PvaError::RemoteError(init.status));
    }
    ioid_guard.arm_destroy(sid);
    let intro = init.introspection;

    // Build the delta from the prototype default, marking only the
    // assigned fields' bits. Shared with the deferred CLI token
    // classifier (`build_put_from_args`) so both produce identical wire
    // deltas for the same assignments.
    let (value, changed) = build_field_delta(&intro, assignments)?;

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
                Err(PvaError::RemoteError(s.status))
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
/// pvxs parity: `pvxs/ioc/pvalink_channel.cpp:31-38` (putReq template carries
/// record options) + `linkBuildPut:138` (field targeting via
/// `top[fieldName]`).
pub async fn op_put_field_with_request(
    channel: &Arc<Channel>,
    field_path: &str,
    pv_req: &[u8],
    value_str: &str,
    op_timeout: Duration,
) -> PvaResult<()> {
    requeue_on_disconnect(channel, op_timeout, |budget| {
        op_put_field_with_request_attempt(channel, field_path, pv_req, value_str, budget)
    })
    .await
}

/// One attempt of [`op_put_field_with_request`]; [`requeue_on_disconnect`] runs it
/// again from the top when the channel is lost mid-op.
async fn op_put_field_with_request_attempt(
    channel: &Arc<Channel>,
    field_path: &str,
    pv_req: &[u8],
    value_str: &str,
    op_timeout: Duration,
) -> PvaResult<()> {
    let (server, sid) = ensure_active_with_op_timeout(channel, op_timeout).await?;
    let order = server.byte_order();
    let big_endian = matches!(order, ByteOrder::Big);
    let codec = PvaCodec { big_endian };
    let ioid = alloc_ioid();

    let mut stream = server.register_ioid_stream(sid, ioid, Command::Put.code());
    let mut ioid_guard = IoidGuard::new(server.clone(), ioid);

    let init_req = codec.build_put_init(sid, ioid, pv_req);
    server.send_for_channel(sid, init_req).await?;
    let init_frame = await_frame(&mut stream, op_timeout).await?;
    let init = match decode_op_or_reset(&server, &init_frame, None)? {
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
        return Err(PvaError::RemoteError(init.status));
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
                Err(PvaError::RemoteError(s.status))
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
/// mirroring pvxs `linkBuildPut` (`pvxs/ioc/pvalink_channel.cpp:138-143`):
/// `auto value(top[fieldName]); if(struct) value = value["value"]`.
///
/// pvxs parity: `pvxs/ioc/pvalink_channel.cpp:127-180` typed array/scalar PUT
/// into the link's `fieldName` target.
pub async fn op_put_value_field_with_request(
    channel: &Arc<Channel>,
    field_path: &str,
    pv_req: &[u8],
    value: &PvField,
    op_timeout: Duration,
) -> PvaResult<()> {
    requeue_on_disconnect(channel, op_timeout, |budget| {
        op_put_value_field_with_request_attempt(channel, field_path, pv_req, value, budget)
    })
    .await
}

/// One attempt of [`op_put_value_field_with_request`]; [`requeue_on_disconnect`] runs it
/// again from the top when the channel is lost mid-op.
async fn op_put_value_field_with_request_attempt(
    channel: &Arc<Channel>,
    field_path: &str,
    pv_req: &[u8],
    value: &PvField,
    op_timeout: Duration,
) -> PvaResult<()> {
    let (server, sid) = ensure_active_with_op_timeout(channel, op_timeout).await?;
    let order = server.byte_order();
    let big_endian = matches!(order, ByteOrder::Big);
    let codec = PvaCodec { big_endian };
    let ioid = alloc_ioid();

    let mut stream = server.register_ioid_stream(sid, ioid, Command::Put.code());
    let mut ioid_guard = IoidGuard::new(server.clone(), ioid);

    let init_req = codec.build_put_init(sid, ioid, pv_req);
    server.send_for_channel(sid, init_req).await?;
    let init_frame = await_frame(&mut stream, op_timeout).await?;
    let init = match decode_op_or_reset(&server, &init_frame, None)? {
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
        return Err(PvaError::RemoteError(init.status));
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
                Err(PvaError::RemoteError(s.status))
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
    requeue_on_disconnect(channel, op_timeout, |budget| {
        op_put_value_attempt(channel, value, budget)
    })
    .await
}

/// One attempt of [`op_put_value`]; [`requeue_on_disconnect`] runs it
/// again from the top when the channel is lost mid-op.
async fn op_put_value_attempt(
    channel: &Arc<Channel>,
    value: &PvField,
    op_timeout: Duration,
) -> PvaResult<()> {
    let (server, sid) = ensure_active_with_op_timeout(channel, op_timeout).await?;
    let order = server.byte_order();
    let big_endian = matches!(order, ByteOrder::Big);
    let codec = PvaCodec { big_endian };
    let ioid = alloc_ioid();

    let pv_req = build_pv_request_value_only(big_endian);
    let mut stream = server.register_ioid_stream(sid, ioid, Command::Put.code());
    let mut ioid_guard = IoidGuard::new(server.clone(), ioid);

    let init_req = codec.build_put_init(sid, ioid, &pv_req);
    server.send_for_channel(sid, init_req).await?;
    let init_frame = await_frame(&mut stream, op_timeout).await?;
    let init = match decode_op_or_reset(&server, &init_frame, None)? {
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
        return Err(PvaError::RemoteError(init.status));
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
                Err(PvaError::RemoteError(s.status))
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
/// bit. pvxs `pvxs/ioc/pvalink_channel.cpp:268` parity for typed OUT arrays.
pub async fn op_put_value_raw(
    channel: &Arc<Channel>,
    pv_req: &[u8],
    value: &PvField,
    op_timeout: Duration,
) -> PvaResult<()> {
    requeue_on_disconnect(channel, op_timeout, |budget| {
        op_put_value_raw_attempt(channel, pv_req, value, budget)
    })
    .await
}

/// One attempt of [`op_put_value_raw`]; [`requeue_on_disconnect`] runs it
/// again from the top when the channel is lost mid-op.
async fn op_put_value_raw_attempt(
    channel: &Arc<Channel>,
    pv_req: &[u8],
    value: &PvField,
    op_timeout: Duration,
) -> PvaResult<()> {
    let (server, sid) = ensure_active_with_op_timeout(channel, op_timeout).await?;
    let order = server.byte_order();
    let big_endian = matches!(order, ByteOrder::Big);
    let codec = PvaCodec { big_endian };
    let ioid = alloc_ioid();

    let mut stream = server.register_ioid_stream(sid, ioid, Command::Put.code());
    let mut ioid_guard = IoidGuard::new(server.clone(), ioid);

    let init_req = codec.build_put_init(sid, ioid, pv_req);
    server.send_for_channel(sid, init_req).await?;
    let init_frame = await_frame(&mut stream, op_timeout).await?;
    let init = match decode_op_or_reset(&server, &init_frame, None)? {
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
        return Err(PvaError::RemoteError(init.status));
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
                Err(PvaError::RemoteError(s.status))
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
    op_put_inner_build(
        channel,
        raw_pv_req,
        op_timeout,
        // No get-first snapshot: the programmatic single-value put has no
        // choice menu to match an enum label against, so it stays one round
        // trip and the value lowers as-is.
        |_intro| false,
        |intro, _previous| {
            Ok((
                build_put_value(intro, value_str)?,
                value_only_bit_set(intro),
            ))
        },
    )
    .await
}

/// Single owner of the PUT INIT → DATA → done wire dance. The value to
/// send is produced by `build_value` *after* the server returns the
/// prototype introspection, so callers whose value parsing depends on
/// the prototype (e.g. the legacy positional array form, where
/// "scalar array" vs "scalar" decides whether the first token is a
/// length to drop) get the prototype before they commit. `build_value`
/// runs synchronously between the INIT reply and the DATA send — no
/// await crosses it.
///
/// When `wants_previous(&intro)` is true, a get-first snapshot is fetched
/// between INIT and the build and handed to `build_value` as
/// `Some(&previous)`. pvput always issues the PUT with get=true
/// (pvput.cpp:409) so it can resolve an enum write by choice label
/// (pvput.cpp:186-188); here the snapshot is fetched only for the
/// prototypes that need it, so an ordinary scalar PUT keeps its single
/// round trip and a write-only PV is not gated on a GET. The snapshot rides
/// the put's own op as pvxs's `GPROp::GetOPut` phase (`subcmd=0x40` on the
/// same `ioid`), not a separate `ChannelGet`. A snapshot that returns an
/// error *status* is best-effort (`None`): the builder still runs and falls
/// back to the no-snapshot path. A transport failure (send / timeout /
/// malformed reply) fails the op instead, because the snapshot shares the
/// op's frame stream and a late reply would desync the exec await.
async fn op_put_inner_build<FB, WP>(
    channel: &Arc<Channel>,
    raw_pv_req: Option<&[u8]>,
    op_timeout: Duration,
    wants_previous: WP,
    build_value: FB,
) -> PvaResult<()>
where
    FB: Fn(&FieldDesc, Option<&PvField>) -> PvaResult<(PvField, BitSet)>,
    WP: Fn(&FieldDesc) -> bool,
{
    requeue_on_disconnect(channel, op_timeout, |budget| {
        op_put_inner_build_attempt(channel, raw_pv_req, budget, &wants_previous, &build_value)
    })
    .await
}

/// One attempt of [`op_put_inner_build`]; [`requeue_on_disconnect`] runs it
/// again from the top when the channel is lost mid-op. The builders are
/// borrowed, not consumed, because a re-issued PUT rebuilds its value
/// against the prototype the *new* channel returns.
async fn op_put_inner_build_attempt<FB, WP>(
    channel: &Arc<Channel>,
    raw_pv_req: Option<&[u8]>,
    op_timeout: Duration,
    wants_previous: &WP,
    build_value: &FB,
) -> PvaResult<()>
where
    FB: Fn(&FieldDesc, Option<&PvField>) -> PvaResult<(PvField, BitSet)>,
    WP: Fn(&FieldDesc) -> bool,
{
    let (server, sid) = ensure_active_with_op_timeout(channel, op_timeout).await?;
    let order = server.byte_order();
    let big_endian = matches!(order, ByteOrder::Big);
    let codec = PvaCodec { big_endian };
    let ioid = alloc_ioid();

    let pv_req = match raw_pv_req {
        Some(b) => b.to_vec(),
        None => build_pv_request_value_only(big_endian),
    };
    let mut stream = server.register_ioid_stream(sid, ioid, Command::Put.code());
    let mut ioid_guard = IoidGuard::new(server.clone(), ioid);

    // INIT
    let init_req = codec.build_put_init(sid, ioid, &pv_req);
    server.send_for_channel(sid, init_req).await?;
    let init_frame = await_frame(&mut stream, op_timeout).await?;
    let init = match decode_op_or_reset(&server, &init_frame, None)? {
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
        return Err(PvaError::RemoteError(init.status));
    }
    ioid_guard.arm_destroy(sid);
    let intro = init.introspection;

    // Get-first snapshot for builders that resolve against the current
    // value (e.g. an enum write matched by choice label). Fetched only when
    // the prototype calls for it. pvput.cpp:409 (get=true) / pvput.cpp:186-188.
    //
    // pvxs reads the snapshot via `GPROp::GetOPut` — a CMD_PUT data-phase
    // frame with `subcmd=0x40` and no value body, on THIS put's own `ioid`,
    // so the server returns the current value through the put's own
    // pvRequest mask before the exec (clientget.cpp:258,299-300,536). The
    // previous code opened a separate `ChannelGet` with an empty all-fields
    // pvRequest — an extra op/RTT, a fresh ioid, and a wider field read than
    // the put mask. An error STATUS reply stays best-effort (`None`): the
    // builder runs and falls back to its no-snapshot path (an enum-by-label
    // build then fails on its own). But because the snapshot now shares this
    // op's frame stream, a transport failure (send / timeout / malformed
    // reply) must fail the op rather than fall back — a late snapshot reply
    // would desync the exec done-frame await. A malformed reply also resets
    // the circuit (pvxs `M.fault()`), via `decode_op_or_reset`.
    let previous = if wants_previous(&intro) {
        let get_oput = codec.build_put_get(sid, ioid);
        server.send_for_channel(sid, get_oput).await?;
        let snap_frame = await_frame(&mut stream, op_timeout).await?;
        match decode_op_or_reset(&server, &snap_frame, Some(&intro))? {
            OpResponse::Data(d) => Some(d.value),
            _ => None,
        }
    } else {
        None
    };

    // Build the value AND its changed-bitset against the prototype. The
    // builder owns the changed-bit decision because only it knows which
    // fields it actually wrote: a bare `.value` write marks the value
    // bit, a deferred field=value delta marks each assigned path's bit.
    let (value, changed) = build_value(&intro, previous.as_ref())?;

    // DATA
    let mut payload = Vec::new();
    payload.put_u32(sid, order);
    payload.put_u32(ioid, order);
    payload.put_u8(0x00);
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
                Err(PvaError::RemoteError(s.status))
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
    /// confirmed our INIT/START. Fires once per connect cycle. Carries
    /// the server endpoint so callers can report `Connected to <peer>`
    /// like pvxs (`tools/monitor.cpp:152`).
    Connected { peer: std::net::SocketAddr },
    /// Server pushed a value update. `value` is the full prior-merged
    /// snapshot; `marked` carries the leaf paths the server flagged
    /// changed in *this* update — the decoded changed `BitSet` resolved
    /// to dotted leaf paths (the shape `format::format_value`'s `marked`
    /// argument expects). It is `None` on the first update of a connect
    /// cycle (a complete snapshot, no prior to delta against) so a delta
    /// renderer shows every leaf, then `Some(set)` on later updates so it
    /// prints only the changed leaves. pvxs renders a monitor delta from
    /// the update's own marked set (`Value::imarked()`, datafmt.cpp:112-120;
    /// the first monitor post is a full value).
    Data {
        intro: FieldDesc,
        value: PvField,
        marked: Option<std::collections::HashSet<String>>,
    },
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
    /// When true, suppress [`MonitorEvent::Disconnected`]. It does NOT
    /// suppress [`MonitorEvent::Finished`]: pvxs gates only the
    /// `Disconnect()` push on `maskDiscon` (clientmon.cpp:397) and pushes
    /// `Finished()` unconditionally on a clean end-of-stream
    /// (clientmon.cpp:706).
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

/// A monitor's CONNECTION-STATE transition — the transition-only subset of
/// [`MonitorEvent`], carried by the handle-based monitors
/// ([`op_monitor_handle`], [`op_monitor_raw_frames_handle`],
/// [`op_monitor_raw_frames_handle_with_request`]).
///
/// **Invariant.** A monitor consumer MUST learn connection transitions from
/// this stream and MUST NOT infer them from the subscription
/// handle/future terminating. The handle loops re-subscribe INTERNALLY on
/// `MonitorEnd::ConnectionLost` (deliver the transition, sleep 200 ms,
/// loop), so a dead upstream never makes the handle's task return — a
/// consumer that watches only the task reports a dead upstream as connected
/// and keeps serving its last value. That was the defect family closed
/// here; [`SubscriptionHandle::wait_terminal`] returns a
/// [`MonitorTermination`], which by type is not a connection state.
///
/// pvxs shape: `pvaLinkChannel` drives its connected/disconnected state from
/// the monitor's event stream and its `catch(client::Disconnect&)` branch
/// (`pvxs/ioc/pvalink_channel.cpp:335-373`), never from a subscription call returning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MonitorConnEvent {
    /// The subscription's channel reached Active and INIT/START were
    /// confirmed. Fires once per connect cycle, carrying the server
    /// endpoint (pvxs `tools/monitor.cpp:152`).
    Connected { peer: std::net::SocketAddr },
    /// The subscription left Active without a clean end-of-stream: the
    /// circuit died, the channel closed, or the loop hit a fatal/remote
    /// error. The loop re-subscribes transparently unless the channel is
    /// closed for good.
    Disconnected,
    /// The server signalled a clean end-of-stream (subcmd `0x10`); no
    /// further updates arrive on this subscription.
    Finished,
}

/// Why a handle monitor's inner loop ended — the return of
/// [`SubscriptionHandle::wait_terminal`].
///
/// **This is NOT a connection-state transition** and must never be read as
/// one: the loop re-subscribes internally on connection loss, so it does not
/// return when the upstream merely goes away. Connect / disconnect
/// transitions come from [`MonitorConnEvent`] — the only sanctioned source.
/// The type exists so that reading is not expressible: there is no
/// `Disconnected` variant to match and no `Err` arm to mistake for one.
#[derive(Debug)]
#[must_use]
pub enum MonitorTermination {
    /// The loop finished for a non-error reason: a clean server
    /// end-of-stream, the channel closed, or [`SubscriptionHandle::stop`]
    /// was called.
    Ended,
    /// The loop died on a fatal (circuit-level) or remote (per-subscription)
    /// error and will not re-subscribe.
    Failed(PvaError),
}

impl MonitorTermination {
    fn from_result(r: PvaResult<()>) -> Self {
        match r {
            Ok(()) => Self::Ended,
            Err(e) => Self::Failed(e),
        }
    }
}

/// The single owner of a handle monitor's connection-state transitions.
///
/// Every [`MonitorConnEvent`] a handle monitor emits goes through this
/// type, and it makes the alternation an invariant BY CONSTRUCTION rather
/// than by convention spread over the loops' match arms:
///
/// * `Connected` is emitted only from the disconnected state, so a
///   reconnect cycle cannot double-announce;
/// * exactly one of `Disconnected` / `Finished` is emitted per `Connected`,
///   so a consumer that treats either as "the upstream is gone" can neither
///   miss an outage nor see a phantom one.
///
/// Both handle loops (typed and raw-frames) drive it identically — one
/// uniform rule, no per-path special case.
struct ConnEventOwner<C> {
    callback: C,
    connected: bool,
}

impl<C: FnMut(MonitorConnEvent)> ConnEventOwner<C> {
    fn new(callback: C) -> Self {
        Self {
            callback,
            connected: false,
        }
    }

    /// The subscription became active on `peer`.
    fn enter_connected(&mut self, peer: std::net::SocketAddr) {
        if self.connected {
            return;
        }
        self.connected = true;
        (self.callback)(MonitorConnEvent::Connected { peer });
    }

    /// The subscription left Active for any reason other than a clean
    /// end-of-stream (circuit lost, channel closed, fatal/remote error).
    fn leave_disconnected(&mut self) {
        if !self.connected {
            return;
        }
        self.connected = false;
        (self.callback)(MonitorConnEvent::Disconnected);
    }

    /// The server closed the stream cleanly (subcmd `0x10`).
    fn leave_finished(&mut self) {
        if !self.connected {
            return;
        }
        self.connected = false;
        (self.callback)(MonitorConnEvent::Finished);
    }
}

/// Per-subscription metrics, mirroring pvxs `SubscriptionStat`
/// (client.h:165-178).
///
/// pvxs's queue fields describe the client-side monitor queue a consumer
/// pops from (`Subscription::pop()`). This client delivers each update
/// through the user callback instead of a `pop()`, but the queue is the
/// same object: the per-IOID backlog
/// (`MonitorBacklog`) the connection
/// reader fills and the monitor loop drains, bounded at `queueSize` and
/// squashed at the tail exactly as pvxs's `std::deque<Entry>` is. So
/// `n_queue`, `n_cli_squash` and `max_queue` carry pvxs's `nQueue`,
/// `nCliSquash` and `queueMax` — they were hardwired 0 while the backlog
/// was an unbounded channel that could neither overflow nor squash.
///
/// The remaining counters are Rust-specific delivery / ACK telemetry pvxs
/// does not define and are named distinctly so they are not mistaken for
/// the pvxs queue surface — in particular `max_events_per_ack` is the
/// ACK-window high-water mark that the previous `max_queue` field
/// conflated with pvxs `maxQueue`.
#[derive(Debug, Clone, Default)]
pub struct SubscriptionStat {
    // ── pvxs `SubscriptionStat` surface ──
    /// pvxs `nQueue`: updates currently queued awaiting delivery — the
    /// depth of this subscription's bounded backlog, sampled as each
    /// update is handed to the callback.
    pub n_queue: u32,
    /// pvxs `nSrvSquash`: count of value updates where the server reported
    /// at least one update dropped/squashed (overrun bitset non-empty).
    /// Populated by the decoded monitor loop. The RAW monitor loop forwards
    /// bytes without decoding the trailing overrun bitset, so raw-handle
    /// stats leave this 0 (the raw stream still carries the overrun bits to
    /// the consumer; see `op_monitor_raw*`).
    pub n_srv_squash: u64,
    /// pvxs `nCliSquash`: updates merged into the queue tail because the
    /// backlog was full — a consumer slower than the update rate.
    pub n_cli_squash: u64,
    /// pvxs `maxQueue`: max client backlog depth observed. For the
    /// ACK-window high-water mark see `max_events_per_ack`.
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
                big_endian: matches!(server.byte_order(), ByteOrder::Big),
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
    task: Option<epics_base_rs::runtime::task::TaskHandle<PvaResult<()>>>,
}

impl SubscriptionHandle {
    /// Pause server emissions on this subscription. Safe to call
    /// multiple times; second call is a no-op when already paused.
    /// Mirrors pvxs `Subscription::pause(true)` (clientmon.cpp:121).
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
            let big_endian = matches!(server.byte_order(), ByteOrder::Big);
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
            let big_endian = matches!(server.byte_order(), ByteOrder::Big);
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

    /// Await the inner task without signalling stop, and report WHY it
    /// ended. Used by long-lived consumers (the bridge gateway) that want
    /// to observe the natural lifetime of the subscription while still
    /// holding a [`Pauser`] cloned out beforehand.
    ///
    /// **Not a disconnect signal.** The loop re-subscribes internally on
    /// `MonitorEnd::ConnectionLost`, so this does NOT return when the
    /// upstream goes away — a consumer that infers "the upstream is gone"
    /// from this returning learns nothing and serves a stale value as good.
    /// Connection transitions come from the handle's [`MonitorConnEvent`]
    /// callback, and [`MonitorTermination`] carries no variant that could
    /// stand in for one. Named `wait_terminal` (and not `wait`) so the
    /// distinction is at the call site, not only in this doc comment.
    pub async fn wait_terminal(mut self) -> MonitorTermination {
        if let Some(t) = self.task.take() {
            match t.await {
                Ok(r) => MonitorTermination::from_result(r),
                Err(_) => MonitorTermination::Ended,
            }
        } else {
            MonitorTermination::Ended
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
            let big_endian = matches!(server.byte_order(), ByteOrder::Big);
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
            let big_endian = matches!(server.byte_order(), ByteOrder::Big);
            let codec = PvaCodec { big_endian };
            let _ = server
                .send_for_channel(sid, codec.build_monitor_resume(sid, ioid))
                .await;
        }
    }
}

/// classification of a frame arriving on the raw MONITOR
/// stream. The control-frame policy lives here as one pure, testable
/// decision so a malformed or out-of-state control frame cannot be
/// silently skipped or reported as a clean end-of-stream — the swallow
/// bugs the raw loop previously had (`payload.len() < 5 => continue`, a
/// FINISH `Status::decode` failure falling through to `Ok(())`, and a
/// second INIT or any unexpected subcmd treated as a benign skip).
#[derive(Debug)]
enum RawMonitorFrameKind {
    /// subcmd `0x00` — a DATA frame; the caller forwards `payload[5..]`.
    Data,
    /// FINISH (`subcmd & 0x10`) carrying a success Status — clean end of
    /// stream.
    FinishOk,
    /// FINISH carrying a success Status AND one last update after it
    /// (`decode::monitor_finish_body`, pvxs `clientmon.cpp:504-511`). The
    /// caller forwards `payload[body_start..]` — the same
    /// `changed | value | overrun` shape a DATA body has — and then ends the
    /// stream, matching pvxs queueing the update ahead of its `Finished()`
    /// marker (`clientmon.cpp:692-707`).
    FinishData { body_start: usize },
    /// An INVALID MONITOR frame: a truncated frame (shorter than `ioid +
    /// subcmd`), a FINISH whose required Status cannot be decoded, a second
    /// INIT (`subcmd & 0x08`) on a running subscription, or any other subcmd
    /// a server never emits on a monitor stream. pvxs faults the buffer and
    /// resets the whole circuit for each of these (`clientmon.cpp:601-605`),
    /// so the caller routes it through [`MonitorTeardown::invalid`].
    Invalid(PvaError),
    /// A well-formed FINISH carrying a NON-SUCCESS Status. This is a remote
    /// error, not a wire fault: pvxs decodes it fine, hands the subscription
    /// a `RemoteError` and leaves the circuit up (`clientmon.cpp:612-614`).
    /// Kept apart from [`Self::Invalid`] so the shared teardown owner cannot
    /// tear down a healthy circuit over an error status.
    FinishError(PvaError),
}

/// classify a raw MONITOR stream frame. Mirrors the typed path's
/// `Status::decode` owner — a missing/malformed FINISH Status is an
/// error, not a clean end. pvxs resets the connection when a monitor
/// message decode is not good (`clientmon.cpp:596`).
fn classify_raw_monitor_frame(payload: &[u8], order: ByteOrder) -> RawMonitorFrameKind {
    // A MONITOR application frame always carries ioid (4) + subcmd (1).
    // A shorter payload is a truncated control frame, not one to skip.
    if payload.len() < 5 {
        return RawMonitorFrameKind::Invalid(PvaError::Decode(format!(
            "MONITOR frame too short: {} bytes (need >= 5 for ioid+subcmd)",
            payload.len()
        )));
    }
    let subcmd = payload[4];
    if subcmd == 0x00 {
        return RawMonitorFrameKind::Data;
    }
    // pvxs reads `init = subcmd & 0x08` (clientmon.cpp:479) and gates on it
    // BEFORE the FINISH/data branches: a monitor that is no longer Creating
    // but receives an INIT subcmd is a state-machine violation — the buffer
    // faults and pvxs resets the connection (clientmon.cpp:589-605). The raw
    // loop runs only post-START (the monitor is Running), so any INIT here is
    // an invalid frame, never a skippable control frame; the caller closes the
    // circuit through `MonitorTeardown::invalid`. Checked before 0x10 to
    // mirror pvxs's init-first precedence.
    if subcmd & 0x08 != 0 {
        return RawMonitorFrameKind::Invalid(PvaError::Protocol(format!(
            "MONITOR INIT (subcmd {subcmd:#04x}) on a running subscription"
        )));
    }
    if subcmd & 0x10 != 0 {
        // FINISH carries a required Status after the subcmd, and MAY append a
        // final update after it. Both questions are answered by the one owner
        // of the FINISH rule so this loop and the typed decode cannot disagree
        // about whether a body exists. A Status that cannot be DECODED is a
        // wire fault (circuit-fatal, like pvxs's `!M.good()`); a Status that
        // decodes to a non-success value is a remote ERROR carried by a
        // well-formed frame — pvxs keeps the circuit and hands the
        // subscription a `RemoteError` (clientmon.cpp:612-614). Neither may
        // degrade to a clean end-of-stream: that would hide an upstream
        // protocol error from a forwarding gateway.
        return match crate::client_native::decode::monitor_finish_body(payload, order) {
            Ok((st, _)) if !st.is_success() => {
                RawMonitorFrameKind::FinishError(PvaError::RemoteError(st))
            }
            Ok((_, Some(body_start))) => RawMonitorFrameKind::FinishData { body_start },
            Ok((_, None)) => RawMonitorFrameKind::FinishOk,
            Err(e) => RawMonitorFrameKind::Invalid(PvaError::Decode(e)),
        };
    }
    // A server emits only DATA (0x00), INIT (0x08), and FINISH (0x10) on a
    // monitor stream (pvxs servermon.cpp:133-149); START/STOP/ACK subcmds
    // are client->server only. Any other subcmd from the server is a
    // protocol violation, not a benign control frame: this loop forwards
    // bodies without decoding, so swallowing it would desync the stream and
    // hide the violation from a forwarding gateway. pvxs decode-faults such a
    // frame and resets the connection (clientmon.cpp:601-605) — the caller
    // does the same via `MonitorTeardown::invalid`.
    RawMonitorFrameKind::Invalid(PvaError::Protocol(format!(
        "MONITOR unexpected subcmd {subcmd:#04x} on a running subscription"
    )))
}

/// Serialize a pvRequest VALUE to its on-wire `descriptor + value`
/// form in `order`. This is the byte shape a MONITOR INIT pvRequest
/// takes (pvxs `clientmon.cpp:345-346` writes `desc(pvRequest)` then
/// `to_wire_full(R, pvRequest)`), so the result is suitable both as a
/// [`crate::codec::PvaCodec::build_monitor_init`] argument and as
/// a stable cache key for "same pvRequest" deduplication. Two equal
/// pvRequest values produce identical bytes for a fixed `order`.
pub fn encode_pv_request_value(req: &PvField, order: ByteOrder) -> Vec<u8> {
    let desc = req.descriptor();
    let mut out = Vec::new();
    encode_type_desc(&desc, order, &mut out);
    encode_pv_field(req, &desc, order, &mut out);
    out
}

/// Extract the `record._options` scalar entries from a decoded pvRequest
/// VALUE into the `(name, ScalarValue)` pairs
/// [`MonitorFlow::from_record_options`] consumes. Mirrors the server's
/// pvRequest navigation (`server_native::tcp::monitor_pipeline_options`):
/// root → `record` → `_options` → scalar leaves. A request with no
/// `record._options` structure (a plain monitor) yields an empty list,
/// which `from_record_options` reads as pipeline-disabled — so a forwarded
/// gateway request that never asked for a pipeline opens a plain upstream
/// monitor, matching pvxs.
fn record_options_from_request(req: &PvField) -> Vec<(String, crate::pvdata::ScalarValue)> {
    let root = match req {
        PvField::Structure(s) => s,
        _ => return Vec::new(),
    };
    let record = match root
        .fields
        .iter()
        .find_map(|(k, v)| (k == "record").then_some(v))
    {
        Some(PvField::Structure(s)) => s,
        _ => return Vec::new(),
    };
    let options = match record
        .fields
        .iter()
        .find_map(|(k, v)| (k == "_options").then_some(v))
    {
        Some(PvField::Structure(s)) => s,
        _ => return Vec::new(),
    };
    options
        .fields
        .iter()
        .filter_map(|(k, v)| match v {
            PvField::Scalar(sv) => Some((k.clone(), sv.clone())),
            _ => None,
        })
        .collect()
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
                epics_base_rs::runtime::task::sleep(std::time::Duration::from_millis(500)).await;
                continue;
            }
        };
        match run_raw_monitor_loop(
            server.clone(),
            sid,
            &fields_owned,
            None,
            MonitorFlow::window(pipeline_size),
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
                epics_base_rs::runtime::task::sleep(std::time::Duration::from_millis(200)).await;
            }
            // Both end the subscription for good — no resubscribe. They differ
            // only in what already happened to the circuit: `Fatal` closed it
            // (pvxs `bev.reset()`), `Remote` left it serving its other
            // channels (pvxs `RemoteError`).
            Err(MonitorEnd::Fatal(e) | MonitorEnd::Remote(e)) => return Err(e),
        }
    }
}

/// Like [`op_monitor_raw_frames`] but returns a
/// [`SubscriptionHandle`] for pause/resume/stats. The inner raw
/// monitor loop runs in a spawned task so the bridge gateway can wire
/// downstream watermark events into upstream pipeline-pause control
/// messages without an intermediate decode/encode pass.
///
/// `on_conn` receives this subscription's connection-state transitions
/// ([`MonitorConnEvent`]) — the ONLY place a consumer may learn that the
/// upstream came or went. It is a required parameter, not an option, so no
/// call site can open a raw monitor with no way to observe a disconnect
/// (the shape that produced the §12.8 defect family).
pub fn op_monitor_raw_frames_handle<F, C>(
    channel: Arc<Channel>,
    fields: &[&str],
    pipeline_size: u32,
    callback: F,
    on_conn: C,
) -> SubscriptionHandle
where
    F: FnMut(&FieldDesc, bytes::Bytes, ByteOrder) + Send + 'static,
    C: FnMut(MonitorConnEvent) + Send + 'static,
{
    let fields_owned: Vec<String> = fields.iter().map(|s| s.to_string()).collect();
    spawn_raw_frames_handle(
        channel,
        fields_owned,
        None,
        pipeline_size,
        callback,
        on_conn,
    )
}

/// Raw-frame monitor handle that forwards a caller-supplied pvRequest
/// VALUE verbatim (re-encoded per upstream connection) instead of
/// deriving the request from a field-name list. The PVA gateway uses
/// this to open an upstream monitor carrying the DOWNSTREAM client's
/// MONITOR INIT pvRequest, so the upstream server applies the same field
/// projection / `record._options._filter` chain the client asked for —
/// pva2pva `p2pApp/channel.cpp:157-193` forwards the serialized
/// downstream pvRequest rather than a gateway-default request, and
/// `moncache.cpp:34-37` caches one upstream monitor per distinct
/// request.
pub fn op_monitor_raw_frames_handle_with_request<F, C>(
    channel: Arc<Channel>,
    pv_request: PvField,
    pipeline_size: u32,
    callback: F,
    on_conn: C,
) -> SubscriptionHandle
where
    F: FnMut(&FieldDesc, bytes::Bytes, ByteOrder) + Send + 'static,
    C: FnMut(MonitorConnEvent) + Send + 'static,
{
    spawn_raw_frames_handle(
        channel,
        Vec::new(),
        Some(pv_request),
        pipeline_size,
        callback,
        on_conn,
    )
}

fn spawn_raw_frames_handle<F, C>(
    channel: Arc<Channel>,
    fields_owned: Vec<String>,
    pv_request: Option<PvField>,
    pipeline_size: u32,
    mut callback: F,
    on_conn: C,
) -> SubscriptionHandle
where
    F: FnMut(&FieldDesc, bytes::Bytes, ByteOrder) + Send + 'static,
    C: FnMut(MonitorConnEvent) + Send + 'static,
{
    // Flow control shares one origin with the wire request: a forwarded
    // pvRequest's own `record._options.{pipeline,queueSize,ackAny}` drive
    // the INIT pipeline bit / `nack` trailer and ACK cadence (pvxs
    // MonitorBuilder::exec, clientmon.cpp:761-808), exactly as the typed
    // `op_monitor_raw` path. A gateway forwarding a downstream request that
    // omits `pipeline=true` therefore opens a plain upstream monitor and
    // sends no ACKs — matching pvxs, whose servers enable pipeline only
    // from the pvRequest (servermon.cpp:523-552). With no forwarded request
    // the client's configured window stands (the auto-built request injects
    // the matching options).
    let flow = match &pv_request {
        Some(req) => {
            MonitorFlow::from_record_options(&record_options_from_request(req), pipeline_size)
        }
        None => MonitorFlow::window(pipeline_size),
    };
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

    let task = channel.reactor().clone().spawn(async move {
        // Single owner of this subscription's connection-state transitions.
        let mut conn = ConnEventOwner::new(on_conn);
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
                    epics_base_rs::runtime::task::sleep(std::time::Duration::from_millis(500))
                        .await;
                    continue;
                }
            };
            conn.enter_connected(server.addr);
            match run_raw_monitor_loop(
                server.clone(),
                sid,
                &fields_owned,
                pv_request.as_ref(),
                flow,
                &mut callback,
                Some(state_for_task.clone()),
            )
            .await
            {
                Ok(()) => {
                    conn.leave_finished();
                    return Ok(());
                }
                Err(MonitorEnd::ChannelClosed) => {
                    conn.leave_disconnected();
                    return Ok(());
                }
                Err(MonitorEnd::ConnectionLost) => {
                    state_for_task.active.lock().take();
                    conn.leave_disconnected();
                    if matches!(
                        channel.current_state(),
                        super::channel::ChannelState::Closed
                    ) {
                        return Ok(());
                    }
                    epics_base_rs::runtime::task::sleep(std::time::Duration::from_millis(200))
                        .await;
                }
                // Both end the subscription for good — no resubscribe. They differ
                // only in what already happened to the circuit: `Fatal` closed it
                // (pvxs `bev.reset()`), `Remote` left it serving its other
                // channels (pvxs `RemoteError`).
                Err(MonitorEnd::Fatal(e) | MonitorEnd::Remote(e)) => {
                    // The subscription left Active. Unlike `op_monitor_events`
                    // — whose caller receives the error in-band because the
                    // future itself resolves `Err` — a handle consumer is
                    // forbidden to read the termination as a connection state
                    // (`MonitorTermination`), so the departure must be
                    // announced here or it is announced nowhere.
                    conn.leave_disconnected();
                    return Err(e);
                }
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
    pv_request: Option<&PvField>,
    flow: MonitorFlow,
    callback: &mut F,
    state: Option<Arc<SubscriptionState>>,
) -> Result<(), MonitorEnd>
where
    F: FnMut(&FieldDesc, bytes::Bytes, ByteOrder) + Send,
{
    let order = server.byte_order();
    let big_endian = matches!(order, ByteOrder::Big);
    let codec = PvaCodec { big_endian };
    let ioid = alloc_ioid();
    // Flow control and the wire request share one origin. A caller-
    // supplied pvRequest is encoded verbatim, and `flow` was derived from
    // that SAME request's `record._options` (see `spawn_raw_frames_handle`
    // / `record_options_from_request`), so the INIT pipeline bit + `nack`
    // trailer and the ACK cadence below cannot disagree with the wire
    // `queueSize`/`pipeline` the server negotiates. Servers enable pipeline
    // only from the pvRequest (pvxs servermon.cpp:523-552), not the INIT
    // subcmd bit, so a forwarded request without `pipeline=true` opens a
    // plain upstream monitor here too. With no caller request the
    // auto-built pvRequest injects the pipeline options iff `flow.pipeline`
    // (the client's configured window).
    let refs: Vec<&str> = fields.iter().map(|s| s.as_str()).collect();
    let pv_req: std::borrow::Cow<'_, [u8]> = match pv_request {
        Some(req) => {
            // Re-encoded per reconnect (carried as a decoded value, not
            // pre-serialized bytes) so a reconnect onto an opposite-endian
            // peer stays correct. pva2pva forwards the serialized
            // downstream pvRequest verbatim (p2pApp/channel.cpp:157-193).
            std::borrow::Cow::Owned(encode_pv_request_value(req, order))
        }
        None if flow.pipeline => {
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
                flow.queue_size,
                big_endian,
            ))
        }
        None if fields.is_empty() => std::borrow::Cow::Borrowed(sentinel_all_fields()),
        None => std::borrow::Cow::Owned(build_pv_request_fields(&refs, big_endian)),
    };
    // MONITOR is the one server-driven slot, so it is the one slot that
    // needs a bound: `queueSize` deep, squashing the tail on overflow
    // (pvxs `clientmon.cpp:52,683-699`). Armed with the INIT
    // introspection below, before START — a squash is a changed-bitset
    // merge and needs the descriptor.
    let stream = server.register_ioid_monitor(
        sid,
        ioid,
        Command::Monitor.code(),
        flow.queue_size as usize,
        flow.pipeline,
    );
    // Single teardown owner for every exit below (see `MonitorTeardown`).
    let td = MonitorTeardown {
        server: &server,
        ioid,
        state: state.as_ref(),
    };
    let init_req =
        codec.build_monitor_init(sid, ioid, &pv_req, flow.pipeline.then_some(flow.queue_size));
    server
        .send_for_channel(sid, init_req)
        .await
        .map_err(|_| td.lost())?;
    // Cancel-aware INIT receive (see `recv_monitor_init`). `active` is not
    // yet published, so a teardown here only unregisters the local IOID
    // and ends ChannelClosed — no DESTROY, matching pvxs `_cancel()` in
    // the Creating phase (clientmon.cpp:810-824).
    let init_frame = match recv_monitor_init(&state, &stream).await {
        MonitorInit::Reply(f) => f,
        MonitorInit::Cancelled => return Err(td.cancelled()),
        MonitorInit::Lost => return Err(td.lost()),
    };
    let init = match decode_op_response(&init_frame, None) {
        Ok(OpResponse::Init(i)) => i,
        // A reply that is not the INIT this Creating monitor expects is a
        // state-machine violation: pvxs faults the buffer and resets the
        // circuit (clientmon.cpp:581-605).
        Ok(other) => {
            return Err(td.invalid(PvaError::Protocol(format!(
                "expected MONITOR INIT, got {other:?}"
            ))));
        }
        // Decode fault in the INIT body — circuit-fatal, exactly as for the
        // one-shot ops (`decode_op_or_reset`).
        Err(e) => return Err(td.invalid(e)),
    };
    if !init.status.is_success() {
        // A non-success INIT Status is data, not a wire fault: pvxs sets
        // `update.exc = RemoteError` and leaves the circuit alive
        // (clientmon.cpp:612-614).
        return Err(td.remote(PvaError::RemoteError(init.status)));
    }
    let intro = init.introspection;
    // The backlog can merge only once it knows the descriptor. Armed
    // HERE — after the INIT reply, before START — so the invariant
    // "a squashable DATA frame implies an armed backlog" holds by the
    // protocol handshake, not by timing: the server may not send DATA
    // before it receives the START below.
    stream.arm(Arc::new(intro.clone()));
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
                    return Err(td.cancelled());
                }
                tokio::select! {
                    biased;
                    _ = s.cancel.notified() => {
                        // The handle's teardown may already have taken
                        // `active` and unregistered the IOID; the owner's
                        // release is idempotent, so both this path and the
                        // no-handle path converge on the same cleanup.
                        return Err(td.cancelled());
                    }
                    f = stream.recv() => match f {
                        Some(f) => f,
                        None => return Err(td.lost()),
                    },
                }
            }
            // No handle: nothing can cancel this loop, so just await the
            // next frame.
            None => match stream.recv().await {
                Some(f) => f,
                None => return Err(td.lost()),
            },
        };
        // classify the frame through the single control-frame
        // owner. A too-short frame and a FINISH with a missing/malformed
        // Status are INVALID — never silently skipped (`continue`) nor
        // degraded to a clean end (`Ok(())`), which would hide upstream
        // protocol corruption from a forwarding gateway.
        // `body_start` is where this frame's `changed | value | overrun` body
        // begins; `final_frame` marks the FINISH that ends the stream after it.
        let (body_start, final_frame) = match classify_raw_monitor_frame(&frame.payload, order) {
            // subcmd 0x00: DATA — body follows ioid+subcmd directly.
            RawMonitorFrameKind::Data => (5, false),
            // FINISH carrying a last update: relay the body, then end — pvxs
            // queues the update before its `Finished()` marker
            // (`clientmon.cpp:504-511,692-707`), so a downstream subscriber
            // must see it rather than have it dropped with the frame.
            RawMonitorFrameKind::FinishData { body_start } => (body_start, true),
            // The owner's release clears the handle's `active` tuple on FINISH
            // so a later `pause()` / `resume()` / `drop()` doesn't act on a
            // (sid, ioid) the client has already unregistered and the server
            // has already finalised. pvxs `clientmon.cpp:720-729` treats
            // FINISH as the operation-owner cleanup path: state→Done, IOID
            // maps erased, no DESTROY sent.
            RawMonitorFrameKind::FinishOk => return td.finished(),
            // Remote error status on a well-formed FINISH — op-local, the
            // circuit keeps serving its other channels.
            RawMonitorFrameKind::FinishError(e) => return Err(td.remote(e)),
            // Invalid MONITOR frame — pvxs `bev.reset()` (clientmon.cpp:601-605).
            RawMonitorFrameKind::Invalid(e) => return Err(td.invalid(e)),
        };
        // Body = changed | value | overrun (raw). Wrap in `Bytes` so the
        // broadcast fan-out shares this allocation refcount-style.
        let body = bytes::Bytes::copy_from_slice(&frame.payload[body_start..]);
        callback(&intro, body, order);
        events_since_ack += 1;
        if final_frame {
            // The relayed update was this stream's last; finish exactly as the
            // status-only FINISH above does (no ACK — the op is over).
            if let Some(s) = &state {
                s.stats.lock().n_delivered += 1;
            }
            return td.finished();
        }
        if let Some(s) = &state {
            let (n_cli_squash, max_queue, n_queue) = stream.counters();
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
            st.n_cli_squash = n_cli_squash;
            st.max_queue = max_queue;
            st.n_queue = n_queue;
            if events_since_ack > st.max_events_per_ack {
                st.max_events_per_ack = events_since_ack;
            }
        }
        if flow.pipeline && events_since_ack >= flow.ack_at {
            let ack = codec.build_monitor_ack(sid, ioid, events_since_ack);
            if server.send_for_channel(sid, ack).await.is_err() {
                return Err(td.lost());
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
    // Adapt the public `FnMut(&FieldDesc, &PvField)` callback to the inner
    // loop's marked-set-carrying signature; these whole-PV / raw monitor
    // APIs deliver the merged value and do not surface the changed set.
    let mut adapter =
        |intro: &FieldDesc,
         value: &PvField,
         _marked: Option<&std::collections::HashSet<String>>| { callback(intro, value) };
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
                epics_base_rs::runtime::task::sleep(std::time::Duration::from_millis(500)).await;
                continue;
            }
        };
        match run_monitor_loop(
            server.clone(),
            sid,
            &fields_owned,
            raw_pv_req.as_deref(),
            flow,
            &mut adapter,
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
                epics_base_rs::runtime::task::sleep(std::time::Duration::from_millis(200)).await;
            }
            // Both end the subscription for good — no resubscribe. They differ
            // only in what already happened to the circuit: `Fatal` closed it
            // (pvxs `bev.reset()`), `Remote` left it serving its other
            // channels (pvxs `RemoteError`).
            Err(MonitorEnd::Fatal(e) | MonitorEnd::Remote(e)) => return Err(e),
        }
    }
}

/// Like [`op_monitor`] but returns a [`SubscriptionHandle`] for
/// pause/resume/stats. The inner monitor loop runs in a spawned task
/// and stops when the handle's `stop()` is called or when the channel
/// is closed.
///
/// `on_conn` receives this subscription's connection-state transitions
/// ([`MonitorConnEvent`]) — the ONLY place a consumer may learn that the
/// upstream came or went. It is a required parameter, not an option, so no
/// call site can open a handle monitor with no way to observe a disconnect
/// (the shape that produced the §12.8 defect family).
pub fn op_monitor_handle<F, C>(
    channel: Arc<Channel>,
    fields: &[&str],
    pipeline_size: u32,
    mut callback: F,
    on_conn: C,
) -> SubscriptionHandle
where
    F: FnMut(&FieldDesc, &PvField) + Send + 'static,
    C: FnMut(MonitorConnEvent) + Send + 'static,
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

    let task = channel.reactor().clone().spawn(async move {
        // Single owner of this subscription's connection-state transitions.
        let mut conn = ConnEventOwner::new(on_conn);
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
                    epics_base_rs::runtime::task::sleep(std::time::Duration::from_millis(500))
                        .await;
                    continue;
                }
            };
            conn.enter_connected(server.addr);
            // Adapt the public `FnMut(&FieldDesc, &PvField)` callback to the
            // inner loop's marked-set-carrying signature; the handle API
            // delivers the merged value and does not surface the changed set.
            let mut adapter =
                |intro: &FieldDesc,
                 value: &PvField,
                 _marked: Option<&std::collections::HashSet<String>>| {
                    callback(intro, value)
                };
            match run_monitor_loop(
                server.clone(),
                sid,
                &fields_owned,
                None,
                flow,
                &mut adapter,
                Some(state_for_task.clone()),
            )
            .await
            {
                Ok(()) => {
                    conn.leave_finished();
                    return Ok(());
                }
                Err(MonitorEnd::ChannelClosed) => {
                    conn.leave_disconnected();
                    return Ok(());
                }
                Err(MonitorEnd::ConnectionLost) => {
                    state_for_task.active.lock().take();
                    conn.leave_disconnected();
                    if matches!(
                        channel.current_state(),
                        super::channel::ChannelState::Closed
                    ) {
                        return Ok(());
                    }
                    epics_base_rs::runtime::task::sleep(std::time::Duration::from_millis(200))
                        .await;
                }
                // Both end the subscription for good — no resubscribe. They differ
                // only in what already happened to the circuit: `Fatal` closed it
                // (pvxs `bev.reset()`), `Remote` left it serving its other
                // channels (pvxs `RemoteError`).
                Err(MonitorEnd::Fatal(e) | MonitorEnd::Remote(e)) => {
                    // See the raw-frames twin: a handle consumer may not read
                    // the termination as a connection state, so the departure
                    // from Active is announced here.
                    conn.leave_disconnected();
                    return Err(e);
                }
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
///
/// `raw_pv_req` is the caller's serialized pvRequest (`None` = the
/// default all-fields request); `flow` carries the negotiated
/// `record._options.{pipeline,queueSize}` window. The descriptor handed
/// to every [`MonitorEvent::Data`] is the monitor's own INIT
/// introspection, so a projected request (`field(alarm)`) yields the
/// projected shape — no separate GET_FIELD is needed.
pub async fn op_monitor_events<F>(
    channel: &Arc<Channel>,
    raw_pv_req: Option<Vec<u8>>,
    flow: MonitorFlow,
    mask: MonitorEventMask,
    mut callback: F,
) -> PvaResult<()>
where
    F: FnMut(MonitorEvent) + Send,
{
    let no_fields: &[String] = &[];
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
                epics_base_rs::runtime::task::sleep(std::time::Duration::from_millis(500)).await;
                continue;
            }
        };
        if !mask.mask_connected {
            callback(MonitorEvent::Connected { peer: server.addr });
        }
        let mut data_callback =
            |intro: &FieldDesc,
             value: &PvField,
             marked: Option<&std::collections::HashSet<String>>| {
                callback(MonitorEvent::Data {
                    intro: intro.clone(),
                    value: value.clone(),
                    marked: marked.cloned(),
                });
            };
        let result = run_monitor_loop(
            server.clone(),
            sid,
            no_fields,
            raw_pv_req.as_deref(),
            flow,
            &mut data_callback,
            None,
        )
        .await;
        match result {
            Ok(()) => {
                // pvxs pushes `Finished()` unconditionally on a clean
                // end-of-stream (clientmon.cpp:701-707); only the
                // `Disconnect()` push below is gated by `maskDiscon`
                // (clientmon.cpp:397). A caller that set mask_disconnected
                // must still receive the legitimate end-of-stream.
                callback(MonitorEvent::Finished);
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
                epics_base_rs::runtime::task::sleep(std::time::Duration::from_millis(200)).await;
            }
            // Both end the subscription for good — no resubscribe. They differ
            // only in what already happened to the circuit: `Fatal` closed it
            // (pvxs `bev.reset()`), `Remote` left it serving its other
            // channels (pvxs `RemoteError`).
            Err(MonitorEnd::Fatal(e) | MonitorEnd::Remote(e)) => return Err(e),
        }
    }
}

/// How a MONITOR loop ended.
///
/// pvxs splits monitor failures into two classes and the port must not blur
/// them:
///
/// * A frame that fails to decode, or that violates the subscription state
///   machine, is a CONNECTION-level protocol violation. pvxs logs "sends
///   invalid MONITOR.  Disconnecting..." and drops the whole virtual circuit
///   — `bev.reset()` (`clientmon.cpp:601-605`) — because the frame was
///   decoded against the connection's shared `rxRegistry` type cache, which a
///   half-decoded frame may already have mutated. Every other channel on that
///   circuit dies with it. → [`MonitorEnd::Fatal`].
/// * A non-success `Status` inside a well-formed reply is *data*, not a
///   fault: pvxs turns it into a per-subscription `RemoteError`
///   (`clientmon.cpp:612-614`) and leaves the circuit serving. →
///   [`MonitorEnd::Remote`].
///
/// `Fatal` is constructible only through [`MonitorTeardown::invalid`], so the
/// invariant *`Fatal` ⟹ the circuit has been closed* holds by construction
/// rather than by each exit path remembering to close.
#[derive(Debug)]
#[allow(dead_code)]
enum MonitorEnd {
    ChannelClosed,
    ConnectionLost,
    /// Op-local: the peer reported an error `Status` on a well-formed frame.
    /// The circuit is intact and still serving its other channels.
    Remote(PvaError),
    /// Circuit-fatal: an invalid MONITOR frame closed the virtual circuit
    /// (pvxs `bev.reset()`).
    Fatal(PvaError),
}

/// The single teardown owner for one MONITOR subscription.
///
/// MUST: every exit from [`run_monitor_loop`] / [`run_raw_monitor_loop`] —
/// clean end, cancel, connection loss, remote error, invalid frame — passes
/// through exactly one method here, which unregisters the IOID and drops the
/// handle's `active` tuple.
///
/// MUST NOT: any other site produce a [`MonitorEnd::Fatal`]. Only
/// [`Self::invalid`] does, and it closes the circuit first — that is what
/// makes the pvxs `bev.reset()` rule (`clientmon.cpp:601-605`) hold for the
/// MONITOR loops the way [`decode_op_or_reset`] already holds it for
/// GET/PUT/RPC/PUT_GET/GET_FIELD/PROCESS.
struct MonitorTeardown<'a> {
    server: &'a ServerConn,
    ioid: u32,
    state: Option<&'a Arc<SubscriptionState>>,
}

impl MonitorTeardown<'_> {
    /// Release this subscription's client-side registrations. Idempotent, and
    /// a no-op for the `active` tuple before the loop publishes it (the INIT
    /// phase), matching pvxs `_cancel()` in the `Creating` state
    /// (`clientmon.cpp:810-824`).
    fn release(&self) {
        self.server.unregister_ioid(self.ioid);
        if let Some(s) = self.state {
            s.active.lock().take();
        }
    }

    /// Clean end of stream (FINISH with a success Status).
    fn finished(&self) -> Result<(), MonitorEnd> {
        self.release();
        Ok(())
    }

    /// The subscriber cancelled (`stop()` / handle dropped).
    fn cancelled(&self) -> MonitorEnd {
        self.release();
        MonitorEnd::ChannelClosed
    }

    /// The circuit went away underneath us; the driver re-searches and
    /// resubscribes.
    fn lost(&self) -> MonitorEnd {
        self.release();
        MonitorEnd::ConnectionLost
    }

    /// Op-local remote error: a well-formed frame carrying a non-success
    /// `Status`. pvxs delivers it as a `RemoteError` to this subscription and
    /// keeps the circuit (`clientmon.cpp:612-614`).
    fn remote(&self, e: PvaError) -> MonitorEnd {
        self.release();
        MonitorEnd::Remote(e)
    }

    /// Circuit-fatal: an undecodable or out-of-state MONITOR frame. pvxs
    /// resets the connection (`clientmon.cpp:601-605`); so do we, before
    /// releasing this subscription.
    fn invalid(&self, e: PvaError) -> MonitorEnd {
        self.server.close();
        self.release();
        MonitorEnd::Fatal(e)
    }
}

/// Resolve a decoded monitor changed `BitSet` to the set of dotted leaf
/// paths it marks — the shape `format::format_value`'s `marked` argument
/// expects (a `Structure` is a container; every other field is a leaf, so
/// a leaf path is `value` / `alarm.severity` / `timeStamp.userTag`).
///
/// `bit_offset` uses the same depth-first numbering as
/// `decode_pv_field_with_bitset` / `fill_unmarked_from_prior` (pvData spec
/// §5.4): a structure occupies its own bit and its children follow. A
/// structure whose own bit is set means the whole subtree was sent fresh
/// (pvData BitSet compression), so every descendant leaf is marked — the
/// same propagation `prune_to_marked` applies. pvxs builds a monitor delta
/// from exactly these marked leaves (`Value::imarked()`, datafmt.cpp:112-120).
fn changed_bitset_to_marked_paths(
    desc: &FieldDesc,
    bitset: &BitSet,
) -> std::collections::HashSet<String> {
    fn walk(
        desc: &FieldDesc,
        bit_offset: usize,
        prefix: &str,
        ancestor_marked: bool,
        bitset: &BitSet,
        out: &mut std::collections::HashSet<String>,
    ) {
        match desc {
            FieldDesc::Structure { fields, .. } => {
                let here_marked = ancestor_marked || bitset.get(bit_offset);
                let mut child_bit = bit_offset + 1;
                for (name, child) in fields {
                    let cpath = if prefix.is_empty() {
                        name.clone()
                    } else {
                        format!("{prefix}.{name}")
                    };
                    walk(child, child_bit, &cpath, here_marked, bitset, out);
                    child_bit += child.total_bits();
                }
            }
            // Leaf (scalar/array/union/any/struct-array): marked iff its own
            // bit or any enclosing structure bit is set.
            _ => {
                if (ancestor_marked || bitset.get(bit_offset)) && !prefix.is_empty() {
                    out.insert(prefix.to_string());
                }
            }
        }
    }
    let mut out = std::collections::HashSet::new();
    walk(desc, 0, "", false, bitset, &mut out);
    out
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
    F: FnMut(&FieldDesc, &PvField, Option<&std::collections::HashSet<String>>) + Send,
{
    let order = server.byte_order();
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

    // MONITOR is the one server-driven slot, so it is the one slot that
    // needs a bound: `queueSize` deep, squashing the tail on overflow
    // (pvxs `clientmon.cpp:52,683-699`). Armed with the INIT
    // introspection below, before START — a squash is a changed-bitset
    // merge and needs the descriptor.
    let stream = server.register_ioid_monitor(
        sid,
        ioid,
        Command::Monitor.code(),
        flow.queue_size as usize,
        flow.pipeline,
    );
    // Single teardown owner for every exit below (see `MonitorTeardown`).
    let td = MonitorTeardown {
        server: &server,
        ioid,
        state: state.as_ref(),
    };

    // INIT — the pipeline bit + initial `nack` (credit window) trailer
    // are written iff `flow.pipeline`, carrying the negotiated
    // `queue_size` (pvxs `clientmon.cpp:333-348` writes `queueSize` only
    // `if(pipeline)`).
    let init_req =
        codec.build_monitor_init(sid, ioid, &pv_req, flow.pipeline.then_some(flow.queue_size));
    server
        .send_for_channel(sid, init_req)
        .await
        .map_err(|_| td.lost())?;
    // Cancel-aware INIT receive (see `recv_monitor_init`). `active` is not
    // yet published, so a teardown here only unregisters the local IOID
    // and ends ChannelClosed — no DESTROY, matching pvxs `_cancel()` in
    // the Creating phase (clientmon.cpp:810-824).
    let init_frame = match recv_monitor_init(&state, &stream).await {
        MonitorInit::Reply(f) => f,
        MonitorInit::Cancelled => return Err(td.cancelled()),
        MonitorInit::Lost => return Err(td.lost()),
    };
    let init = match decode_op_response(&init_frame, None) {
        Ok(OpResponse::Init(i)) => i,
        // A reply that is not the INIT this Creating monitor expects is a
        // state-machine violation: pvxs faults the buffer and resets the
        // circuit (clientmon.cpp:581-605).
        Ok(other) => {
            return Err(td.invalid(PvaError::Protocol(format!(
                "expected MONITOR INIT, got {other:?}"
            ))));
        }
        // Decode fault in the INIT body — circuit-fatal, exactly as for the
        // one-shot ops (`decode_op_or_reset`).
        Err(e) => return Err(td.invalid(e)),
    };
    if !init.status.is_success() {
        // A non-success INIT Status is data, not a wire fault: pvxs sets
        // `update.exc = RemoteError` and leaves the circuit alive
        // (clientmon.cpp:612-614).
        return Err(td.remote(PvaError::RemoteError(init.status)));
    }
    let intro = init.introspection;
    // Same arming point as the raw loop: after the INIT reply, before
    // START, so a squashable DATA frame always finds the descriptor.
    stream.arm(Arc::new(intro.clone()));

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
                    return Err(td.cancelled());
                }
                tokio::select! {
                    biased;
                    _ = s.cancel.notified() => {
                        // The handle's teardown may already have taken
                        // `active` and unregistered the IOID; the owner's
                        // release is idempotent, so both this path and the
                        // no-handle path converge on the same cleanup.
                        return Err(td.cancelled());
                    }
                    f = stream.recv() => match f {
                        Some(f) => f,
                        None => return Err(td.lost()),
                    },
                }
            }
            // No handle: nothing can cancel this loop, so just await the
            // next frame.
            None => match stream.recv().await {
                Some(f) => f,
                None => return Err(td.lost()),
            },
        };
        // Decode DATA with no shared cache. The reader side
        // (`flatten_type_cache_markers`) has already flattened every
        // `0xFD`/`0xFE` type-cache marker — both the INIT descriptor and
        // any `any`/`variant` value markers — into a single self-contained
        // frame in wire order, so this DATA frame embeds its own inline types
        // and needs no connection-level registry to resolve.
        let decoded = decode_op_response(&frame, Some(&intro));
        match decoded {
            Ok(OpResponse::Data(d)) => {
                // a non-empty overrun bitset means the server
                // coalesced updates because we fell behind. Capture it
                // before `d.value` is moved below.
                let srv_squash = !d.overrun.is_empty();
                // pvxs renders a monitor delta from the update's own changed
                // set (`Value::imarked()`, datafmt.cpp:112-120). The first
                // post of a connect cycle is a complete snapshot with no
                // prior to delta against, so surface marked=None (every leaf
                // shown); later posts carry only the server-marked changed
                // leaves. A non-structure top has no addressable leaves —
                // the value itself is the datum, always shown — so leave it
                // None too (the delta formatter hides a struct-less top when
                // a marked subset is supplied). Resolve before `d.value`
                // moves; `d.changed` is still borrowable afterwards.
                let marked = if prior.is_some() && matches!(intro, FieldDesc::Structure { .. }) {
                    Some(changed_bitset_to_marked_paths(&intro, &d.changed))
                } else {
                    None
                };
                let value = if let Some(prev) = prior.as_ref() {
                    crate::pvdata::encode::fill_unmarked_from_prior(
                        &intro, &d.changed, 0, d.value, prev,
                    )
                } else {
                    d.value
                };
                prior = Some(value.clone());
                callback(&intro, &value, marked.as_ref());
                events_since_ack += 1;
                if let Some(s) = &state {
                    let (n_cli_squash, max_queue, n_queue) = stream.counters();
                    let mut st = s.stats.lock();
                    st.n_delivered += 1;
                    if srv_squash {
                        st.n_srv_squash += 1;
                    }
                    st.n_cli_squash = n_cli_squash;
                    st.max_queue = max_queue;
                    st.n_queue = n_queue;
                    if events_since_ack > st.max_events_per_ack {
                        st.max_events_per_ack = events_since_ack;
                    }
                }
                // (d was destructured above when computing `value`.)
                // A FINISH frame that carried a trailing update decodes as
                // DATA with the final bit still set. pvxs queues that update
                // and then appends the `Finished()` marker
                // (`clientmon.cpp:504-511,701-707`): the subscriber sees the
                // last value, then end-of-stream. Deliver-then-end here, with
                // no ACK — the operation is over.
                if d.subcmd & 0x10 != 0 {
                    return td.finished();
                }
                if flow.pipeline && events_since_ack >= flow.ack_at {
                    let ack = codec.build_monitor_ack(sid, ioid, events_since_ack);
                    if server.send_for_channel(sid, ack).await.is_err() {
                        return Err(td.lost());
                    }
                    if let Some(s) = &state {
                        s.stats.lock().n_acks += 1;
                    }
                    events_since_ack = 0;
                }
            }
            Ok(OpResponse::Status(s)) => {
                if s.status.is_success() {
                    return td.finished();
                }
                // A non-success Status on a well-formed frame is data, not a
                // wire fault: pvxs delivers it as a per-subscription
                // `RemoteError` and keeps the circuit (clientmon.cpp:612-614).
                return Err(td.remote(PvaError::RemoteError(s.status)));
            }
            Ok(OpResponse::Init(_)) => {
                // A second INIT while the monitor is already Running is a
                // state-machine violation. pvxs accepts only Creating+INIT,
                // Idle+non-INIT, or Running+non-INIT, and resets the
                // connection otherwise (clientmon.cpp:568-605) — so this goes
                // through the same `invalid` owner the raw path's
                // `RawMonitorFrameKind::Invalid` uses, instead of being
                // treated as harmless.
                return Err(td.invalid(PvaError::Protocol(
                    "MONITOR: unexpected second INIT on a running subscription".into(),
                )));
            }
            Err(e) => {
                // A decode fault on a post-INIT MONITOR frame (truncated DATA
                // body, missing trailing overrun bitset, malformed FINISH
                // Status) is a CONNECTION-level protocol fault: pvxs logs
                // "sends invalid MONITOR.  Disconnecting..." and drops the
                // circuit — `bev.reset()` (clientmon.cpp:601-605). The frame
                // was decoded against the connection's shared reader type
                // cache, so a half-decoded frame can leave that cache
                // mutated; ending only THIS subscription would leave the
                // circuit serving its other channels from it. Route it
                // through the teardown owner, which closes the circuit first.
                // (Ending only the subscription also desyncs pipeline ACK
                // accounting — the skipped frame's credit is never returned,
                // stalling a window-limited server.)
                return Err(td.invalid(PvaError::Protocol(format!("MONITOR decode error: {e}"))));
            }
        }
    }
}

// ── RPC ────────────────────────────────────────────────────────────────

/// RPC DATA-phase argument.
///
/// pvxs distinguishes a *top-level null* argument
/// (`Context::rpc(name, Value())`) from a present value whose type is
/// `any`. The null argument serializes as the single `0xff` "null
/// type" descriptor tag with no value body
/// (`dataencode.cpp:30-35` + `clientget.cpp:307-311` — the value body
/// is written only when the arg is valid); a present argument
/// serializes as `type(arg) + full_value(arg)`. This is distinct from
/// `FieldDesc::Variant` plus an empty `PvField::Variant`, which is the
/// "present `any` whose selected value is null" shape.
#[derive(Clone, Copy)]
pub enum RpcArg<'a> {
    /// pvxs top-level null argument — the single `0xff` type tag, no
    /// value body.
    Null,
    /// A present argument carrying its own descriptor and value.
    Typed {
        desc: &'a FieldDesc,
        value: &'a PvField,
    },
}

/// Serialize the RPC DATA-phase argument — the single owner of the
/// RPC-argument wire shape, so the null/typed distinction lives in
/// exactly one place. pvxs `clientget.cpp:307-311`.
fn encode_rpc_exec_arg(arg: &RpcArg<'_>, order: ByteOrder, out: &mut Vec<u8>) {
    match arg {
        // pvxs `dataencode.cpp:30-35` — a null `FieldDesc*` is the
        // single `0xff` byte; no value body follows.
        RpcArg::Null => out.put_u8(0xff),
        RpcArg::Typed { desc, value } => {
            encode_type_desc(desc, order, out);
            encode_pv_field(value, desc, order, out);
        }
    }
}

pub async fn op_rpc(
    channel: &Arc<Channel>,
    request_desc: &FieldDesc,
    request_value: &PvField,
    arg: RpcArg<'_>,
    op_timeout: Duration,
) -> PvaResult<RpcReply> {
    requeue_on_disconnect(channel, op_timeout, |budget| {
        op_rpc_attempt(channel, request_desc, request_value, arg, budget)
    })
    .await
}

/// One attempt of [`op_rpc`]; [`requeue_on_disconnect`] runs it
/// again from the top when the channel is lost mid-op.
async fn op_rpc_attempt(
    channel: &Arc<Channel>,
    request_desc: &FieldDesc,
    request_value: &PvField,
    arg: RpcArg<'_>,
    op_timeout: Duration,
) -> PvaResult<RpcReply> {
    let (server, sid) = ensure_active_with_op_timeout(channel, op_timeout).await?;
    let order = server.byte_order();
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
        return Err(PvaError::RemoteError(init_resp.status));
    }
    ioid_guard.arm_destroy(sid);
    let response_intro = init_resp.introspection;

    // DATA — RPC argument. pvxs clientget.cpp:307-311 writes the
    // argument descriptor then the value body *only when the arg is
    // valid*; a top-level null arg is the single 0xff null-type tag
    // with no body. `encode_rpc_exec_arg` owns that distinction.
    let mut data = Vec::new();
    data.put_u32(sid, order);
    data.put_u32(ioid, order);
    data.put_u8(0x00);
    encode_rpc_exec_arg(&arg, order, &mut data);
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
                // An RPC DATA response carries its own type descriptor, so
                // `response_desc == None` here means the server sent the NULL
                // (`0xFF`) type code — pvxs's no-argument `ExecOp::reply()`,
                // which its client decodes to an empty `Value`
                // (`clientget.cpp:415-421`).
                Ok(match d.response_desc {
                    Some(desc) => RpcReply::Value(desc, d.value),
                    None => RpcReply::Empty,
                })
            } else {
                Err(PvaError::RemoteError(d.status))
            }
        }
        OpResponse::Status(s) => Err(PvaError::RemoteError(s.status)),
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
    // Default putGet (data subcmd 0x00): write `value_str`, then read back.
    op_put_get_data(channel, 0x00, None, PutGetPut::Str(value_str), op_timeout)
        .await
        .map(|r| (r.desc, r.value))
}

/// Atomic `PUT_GET` (cmd 12) writing a typed [`PvField`] with the default
/// `field(value)` pvRequest — the typed-value counterpart of
/// [`op_put_get`] (which parses a string). Returns the get-leg
/// `(FieldDesc, PvField)` readback.
pub async fn op_put_get_value(
    channel: &Arc<Channel>,
    value: &PvField,
    op_timeout: Duration,
) -> PvaResult<(FieldDesc, PvField)> {
    op_put_get_data(channel, 0x00, None, PutGetPut::Typed(value), op_timeout)
        .await
        .map(|r| (r.desc, r.value))
}

/// [`op_put_get_value`] keeping the readback's marked leaves. See
/// [`MarkedRead`].
pub async fn op_put_get_value_marked(
    channel: &Arc<Channel>,
    value: &PvField,
    op_timeout: Duration,
) -> PvaResult<MarkedRead> {
    op_put_get_data(channel, 0x00, None, PutGetPut::Typed(value), op_timeout).await
}

/// Atomic `PUT_GET` (cmd 12) carrying a caller-supplied pvRequest and a
/// typed [`PvField`]. INIT sends `pv_req` bytes (e.g. a PVA gateway's
/// preserved downstream `ChannelContext.pv_request`, carrying
/// `record._options.process`/`block`) instead of the default
/// `field(value)` selector; the put leg encodes the typed `value`.
/// Returns the get-leg `(FieldDesc, PvField)` readback.
///
/// pva2pva `p2pApp/channel.cpp:129-138`: `GWChannel::createChannelPutGet`
/// forwards the original pvRequest verbatim to the upstream channel and
/// returns its readback — one upstream operation, not a local put plus a
/// separately-read cached get.
pub async fn op_put_get_value_raw(
    channel: &Arc<Channel>,
    pv_req: &[u8],
    value: &PvField,
    op_timeout: Duration,
) -> PvaResult<(FieldDesc, PvField)> {
    op_put_get_value_raw_marked(channel, pv_req, value, op_timeout)
        .await
        .map(|r| (r.desc, r.value))
}

/// [`op_put_get_value_raw`] keeping the readback's marked leaves. See
/// [`MarkedRead`].
pub async fn op_put_get_value_raw_marked(
    channel: &Arc<Channel>,
    pv_req: &[u8],
    value: &PvField,
    op_timeout: Duration,
) -> PvaResult<MarkedRead> {
    op_put_get_data(
        channel,
        0x00,
        Some(pv_req),
        PutGetPut::Typed(value),
        op_timeout,
    )
    .await
}

/// EPICS ChannelPutGet `getGet` (`QOS_GET`, 0x40): read the current
/// get-side data with no put. INITs a PUT_GET op, sends a payload-less
/// getGet data frame, and returns the readback (pvAccess
/// `clientContextImpl.cpp:1100`, `:1138-1152`).
pub async fn op_get_get(
    channel: &Arc<Channel>,
    op_timeout: Duration,
) -> PvaResult<(FieldDesc, PvField)> {
    op_put_get_data(channel, QosFlags::GET, None, PutGetPut::None, op_timeout)
        .await
        .map(|r| (r.desc, r.value))
}

/// EPICS ChannelPutGet `getPut` (`QOS_GET_PUT`, 0x80): read the current
/// put-side data with no put (pvAccess `clientContextImpl.cpp:1156-1170`).
pub async fn op_get_put(
    channel: &Arc<Channel>,
    op_timeout: Duration,
) -> PvaResult<(FieldDesc, PvField)> {
    op_put_get_data(
        channel,
        QosFlags::GET_PUT,
        None,
        PutGetPut::None,
        op_timeout,
    )
    .await
    .map(|r| (r.desc, r.value))
}

/// [`op_get_get`] carrying a caller-supplied pvRequest. The INIT pvRequest
/// can carry a `getField(...)` selector so the readback projects the get-leg
/// structure (pvDatabaseCPP `ChannelPutGetLocal`,
/// modules/pvDatabase/src/pvAccess/channelLocal.cpp); without one the server
/// falls back to the common `field` selection. The default
/// [`op_get_get`] sends only the value-only selector and so cannot express a
/// distinct get-leg.
pub async fn op_get_get_with_request(
    channel: &Arc<Channel>,
    pv_req: &[u8],
    op_timeout: Duration,
) -> PvaResult<(FieldDesc, PvField)> {
    op_put_get_data(
        channel,
        QosFlags::GET,
        Some(pv_req),
        PutGetPut::None,
        op_timeout,
    )
    .await
    .map(|r| (r.desc, r.value))
}

/// [`op_get_put`] carrying a caller-supplied pvRequest. The INIT pvRequest
/// can carry a `putField(...)` selector so the readback projects the put-leg
/// structure (pvDatabaseCPP `ChannelPutGetLocal::getPut`,
/// modules/pvDatabase/src/pvAccess/channelLocal.cpp); without one the server
/// falls back to the common `field` selection.
pub async fn op_get_put_with_request(
    channel: &Arc<Channel>,
    pv_req: &[u8],
    op_timeout: Duration,
) -> PvaResult<(FieldDesc, PvField)> {
    op_put_get_data(
        channel,
        QosFlags::GET_PUT,
        Some(pv_req),
        PutGetPut::None,
        op_timeout,
    )
    .await
    .map(|r| (r.desc, r.value))
}

/// The put leg of a [`op_put_get_data`] round trip. The read-only
/// getGet/getPut subcommands carry no payload (`None`); the default
/// putGet writes a string-parsed value (`Str`); the typed putGet (gateway
/// forward / typed clients) writes a pre-built [`PvField`] (`Typed`). One
/// enum so every PUT_GET variant shares the single INIT/data/destroy
/// lifecycle below and cannot drift apart.
#[derive(Clone, Copy)]
enum PutGetPut<'a> {
    None,
    Str(&'a str),
    Typed(&'a PvField),
}

/// Shared PUT_GET data-phase driver: INIT (with `req` bytes when the
/// caller supplies a pvRequest, else the default `field(value)`
/// selector), then a single data frame whose subcommand selects putGet
/// (`Str`/`Typed` → put BitSet + value) or the read-only getGet/getPut
/// (`None` → no payload, pvAccess `clientContextImpl.cpp:1100-1112`), then
/// decode the readback. Factored so every subcommand and value form shares
/// one INIT/destroy lifecycle and cannot drift apart.
async fn op_put_get_data(
    channel: &Arc<Channel>,
    data_subcmd: u8,
    req: Option<&[u8]>,
    put: PutGetPut<'_>,
    op_timeout: Duration,
) -> PvaResult<MarkedRead> {
    requeue_on_disconnect(channel, op_timeout, |budget| {
        op_put_get_data_attempt(channel, data_subcmd, req, put, budget)
    })
    .await
}

/// One attempt of [`op_put_get_data`]; [`requeue_on_disconnect`] runs it
/// again from the top when the channel is lost mid-op.
async fn op_put_get_data_attempt(
    channel: &Arc<Channel>,
    data_subcmd: u8,
    req: Option<&[u8]>,
    put: PutGetPut<'_>,
    op_timeout: Duration,
) -> PvaResult<MarkedRead> {
    let (server, sid) = ensure_active_with_op_timeout(channel, op_timeout).await?;
    let order = server.byte_order();
    let big_endian = matches!(order, ByteOrder::Big);
    let codec = PvaCodec { big_endian };
    let ioid = alloc_ioid();

    // INIT pvRequest: the caller's bytes (preserved downstream request)
    // when supplied, else the default value-only `field(value)` selector.
    let default_req;
    let pv_req: &[u8] = match req {
        Some(b) => b,
        None => {
            default_req = build_pv_request_value_only(big_endian);
            &default_req
        }
    };
    let mut stream = server.register_ioid_stream(sid, ioid, Command::PutGet.code());
    let mut ioid_guard = IoidGuard::new(server.clone(), ioid);
    // Op-local type cache. PUT_GET and ChannelArray run their INIT and DATA
    // legs on one task in wire order, so this cache resolves any within-op
    // 0xFD/0xFE markers without a shared connection cache. PUT_GET's DATA
    // value is additionally reader-flattened (its layout is unambiguous), so
    // here it stays empty; ChannelArray's ambiguous DATA layout keeps it as
    // the op's resolver — see `flatten_type_cache_markers`.
    let mut cache = crate::pvdata::encode::TypeCache::new();

    // INIT — `sid + ioid + 0x08 + pvRequest`.
    let mut init = Vec::with_capacity(9 + pv_req.len());
    init.put_u32(sid, order);
    init.put_u32(ioid, order);
    init.put_u8(QosFlags::INIT);
    init.extend_from_slice(pv_req);
    let init_h = PvaHeader::application(false, order, Command::PutGet.code(), init.len() as u32);
    let mut init_frame = Vec::with_capacity(8 + init.len());
    init_h.write_into(&mut init_frame);
    init_frame.extend_from_slice(&init);
    server.send_for_channel(sid, init_frame).await?;

    let init_resp = await_frame(&mut stream, op_timeout).await?;
    // pvAccessCPP keeps two client containers after INIT (`m_putData` from
    // putIF, `m_getData` from getIF, clientContextImpl.cpp:1036-1040): the
    // put leg is built/serialized against putIF, the get-side readback is
    // decoded against getIF, and `getPut` reads put-side data back through
    // putIF (:1156-1170). Using one descriptor for both breaks against a
    // server whose put and get structures differ.
    let (put_if, get_if) = match decode_put_get_init(&init_resp, &mut cache) {
        Ok(Ok(descs)) => descs,
        Ok(Err(status)) => {
            server.unregister_ioid(ioid);
            return Err(PvaError::RemoteError(status));
        }
        Err(e) => {
            // Command/subcommand mismatch or truncated INIT body is fatal.
            server.close();
            return Err(e);
        }
    };
    ioid_guard.arm_destroy(sid);

    // Data — `sid + ioid + subcmd [+ put bitset + put value]`. putGet
    // (`Some`) carries a payload built against the PUT descriptor;
    // getGet/getPut (`None`) send none.
    let mut data = Vec::new();
    data.put_u32(sid, order);
    data.put_u32(ioid, order);
    data.put_u8(data_subcmd);
    // putGet carries a put payload (string-parsed or typed) built against
    // the PUT descriptor; getGet/getPut (`None`) send none.
    let put_value = match put {
        PutGetPut::None => None,
        PutGetPut::Str(value_str) => Some(build_put_value(&put_if, value_str)?),
        PutGetPut::Typed(value) => Some(coerce_typed_put_value(&put_if, value)?),
    };
    if let Some(value) = put_value {
        let mut changed = BitSet::new();
        if let Some(bit) = put_if.bit_for_path("value") {
            changed.set(bit);
        } else {
            changed.set(0);
        }
        changed.write_into(order, &mut data);
        // pvxs `from_wire_valid` decodes a BitSet delta — only the fields
        // whose bit is set. Encode consistently against the PUT descriptor.
        encode_pv_field_with_bitset(&value, &put_if, &changed, 0, order, &mut data);
    }
    let data_h = PvaHeader::application(false, order, Command::PutGet.code(), data.len() as u32);
    let mut data_frame = Vec::with_capacity(8 + data.len());
    data_h.write_into(&mut data_frame);
    data_frame.extend_from_slice(&data);
    server.send_for_channel(sid, data_frame).await?;

    let resp_frame = await_frame(&mut stream, op_timeout).await?;
    // getPut (`QOS_GET_PUT`) reads the PUT-side data, so decode and return
    // the PUT descriptor; putGet and getGet read GET-side data → getIF.
    let is_get_put = data_subcmd & QosFlags::GET_PUT != 0;
    let result = {
        let resp_desc = if is_get_put { &put_if } else { &get_if };
        match decode_put_get_data(&resp_frame, resp_desc, &mut cache) {
            Ok(Ok(decoded)) => Ok(decoded),
            Ok(Err(status)) => Err(PvaError::RemoteError(status)),
            Err(e) => {
                // Command mismatch or truncated data body is fatal.
                server.close();
                Err(e)
            }
        }
    };

    ioid_guard.disarm();
    let destroy = codec.build_destroy_request(sid, ioid);
    let _ = server.send_for_channel(sid, destroy).await;
    server.unregister_ioid(ioid);
    let resp_desc = if is_get_put { put_if } else { get_if };
    result.map(|(changed, value)| {
        let marked = reply_marks(&resp_desc, &changed);
        MarkedRead {
            desc: resp_desc,
            value,
            marked,
        }
    })
}

/// Decode a `PUT_GET` INIT response: `ioid + subcmd + status + putIF +
/// getIF`. On success returns BOTH leg descriptors as `(put_if, get_if)`.
///
/// pvAccessCPP keeps the two as separate client containers (`m_putData` /
/// `m_getData`, clientContextImpl.cpp:1036-1040): the put leg is built and
/// serialized against `putIF`, the get-side readback is deserialized
/// against `getIF`, and a `getPut` reads the put-side data back through
/// `putIF`. Returning only `getIF` (and using it for the put leg too) is
/// the descriptor-selection bug this fixes — it breaks against a server
/// whose put and get structures differ.
///
/// The two-level result separates connection-fatal faults from per-op
/// failures: an outer `Err` (command/subcommand mismatch, truncated body)
/// is a protocol violation pvxs answers with `bev.reset()`
/// (clientget.cpp:456-493); an inner `Ok(Err(status))` is a non-success
/// INIT — a per-operation error the caller surfaces without resetting.
fn decode_put_get_init(
    frame: &super::decode::Frame,
    type_cache: &mut crate::pvdata::encode::TypeCache,
) -> PvaResult<Result<(FieldDesc, FieldDesc), crate::proto::Status>> {
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
    // putIF then getIF. Both are decoded (advancing the cursor +
    // populating the type cache) and BOTH are returned: the put leg uses
    // putIF, the get-side readback uses getIF, and getPut reads put-side
    // data back through putIF (pvAccessCPP m_putData / m_getData).
    let put_if = crate::pvdata::encode::decode_type_desc_cached(&mut cur, order, type_cache)
        .map_err(|e| PvaError::Decode(e.to_string()))?;
    let get_if = crate::pvdata::encode::decode_type_desc_cached(&mut cur, order, type_cache)
        .map_err(|e| PvaError::Decode(e.to_string()))?;
    Ok(Ok((put_if, get_if)))
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
) -> PvaResult<Result<(BitSet, PvField), crate::proto::Status>> {
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
    Ok(Ok((changed, value)))
}

// ── ARRAY (cmd 14) — ChannelArray windowed-array operation ──────────────

/// One ChannelArray data-phase sub-request, mirroring the QOS-bit selection
/// in pvAccessCPP `clientContextImpl.cpp:1580-1612`.
#[derive(Clone, Copy)]
enum ArrayReq<'a> {
    /// `getArray` (`QOS_GET`): read the `[offset, count, stride]` slice.
    Get {
        offset: u32,
        count: u32,
        stride: u32,
    },
    /// `putArray` (subcmd 0): splice `value` at `offset`/`stride`.
    Put {
        offset: u32,
        stride: u32,
        value: &'a PvField,
    },
    /// `setLength` (`QOS_GET_PUT`): resize to `length`.
    SetLength { length: u32 },
    /// `getLength` (`QOS_PROCESS`): query the element count.
    GetLength,
}

/// The decoded body of a ChannelArray sub-op reply.
enum ArrayResp {
    /// `getArray`: the sliced array value.
    Value(PvField),
    /// `getLength`: the element count.
    Length(u32),
    /// `putArray` / `setLength`: status-only success.
    Empty,
}

/// Shared ChannelArray driver: INIT (binding the array field via a
/// `field(value)` pvRequest), one sub-op data frame, decode the reply, then
/// DESTROY_REQUEST. Factored so every sub-op shares one INIT/destroy
/// lifecycle and cannot drift apart (mirrors [`op_put_get_data`]).
///
/// Wire format — pvAccessCPP `clientContextImpl.cpp:1567-1666` (send) and
/// `responseHandlers.cpp:2347-2393` (server reply):
/// - INIT reply: `ioid + subcmd + status + array introspection`.
/// - getArray: data `offset + count + stride`; reply `status + array value`.
/// - putArray: data `offset + stride + array value`; reply `status`.
/// - setLength: data `length`; reply `status`.
/// - getLength: data (none); reply `status + length`.
async fn op_array_data(
    channel: &Arc<Channel>,
    pv_request: Option<&PvField>,
    req: ArrayReq<'_>,
    op_timeout: Duration,
) -> PvaResult<(FieldDesc, ArrayResp)> {
    requeue_on_disconnect(channel, op_timeout, |budget| {
        op_array_data_attempt(channel, pv_request, req, budget)
    })
    .await
}

/// One attempt of [`op_array_data`]; [`requeue_on_disconnect`] runs it
/// again from the top when the channel is lost mid-op.
async fn op_array_data_attempt(
    channel: &Arc<Channel>,
    pv_request: Option<&PvField>,
    req: ArrayReq<'_>,
    op_timeout: Duration,
) -> PvaResult<(FieldDesc, ArrayResp)> {
    let (server, sid) = ensure_active_with_op_timeout(channel, op_timeout).await?;
    let order = server.byte_order();
    let big_endian = matches!(order, ByteOrder::Big);
    let codec = PvaCodec { big_endian };
    let ioid = alloc_ioid();

    // INIT pvRequest: forward the caller's verbatim (the gateway's
    // preserved downstream ARRAY pvRequest, which selects the bound array
    // field) or the default `field(value)` selection when none was supplied
    // (pvAccessCPP `clientContextImpl.cpp:1567` sends the create-time
    // pvRequest on the QOS_INIT frame).
    let pv_req = match pv_request {
        Some(req) => encode_pv_request_value(req, order),
        None => build_pv_request_value_only(big_endian),
    };
    let mut stream = server.register_ioid_stream(sid, ioid, Command::Array.code());
    let mut ioid_guard = IoidGuard::new(server.clone(), ioid);
    // Op-local type cache. PUT_GET and ChannelArray run their INIT and DATA
    // legs on one task in wire order, so this cache resolves any within-op
    // 0xFD/0xFE markers without a shared connection cache. PUT_GET's DATA
    // value is additionally reader-flattened (its layout is unambiguous), so
    // here it stays empty; ChannelArray's ambiguous DATA layout keeps it as
    // the op's resolver — see `flatten_type_cache_markers`.
    let mut cache = crate::pvdata::encode::TypeCache::new();

    // INIT — `sid + ioid + 0x08 + pvRequest`.
    let mut init = Vec::with_capacity(9 + pv_req.len());
    init.put_u32(sid, order);
    init.put_u32(ioid, order);
    init.put_u8(QosFlags::INIT);
    init.extend_from_slice(&pv_req);
    let init_h = PvaHeader::application(false, order, Command::Array.code(), init.len() as u32);
    let mut init_frame = Vec::with_capacity(8 + init.len());
    init_h.write_into(&mut init_frame);
    init_frame.extend_from_slice(&init);
    server.send_for_channel(sid, init_frame).await?;

    let init_resp = await_frame(&mut stream, op_timeout).await?;
    let array_desc = match decode_array_init(&init_resp, &mut cache) {
        Ok(Ok(desc)) => desc,
        Ok(Err(status)) => {
            server.unregister_ioid(ioid);
            return Err(PvaError::RemoteError(status));
        }
        Err(e) => {
            server.close();
            return Err(e);
        }
    };
    ioid_guard.arm_destroy(sid);

    // Data frame — `sid + ioid + subcmd [+ params]`.
    let (data_subcmd, body): (u8, Vec<u8>) = match &req {
        ArrayReq::Get {
            offset,
            count,
            stride,
        } => {
            let mut b = Vec::new();
            crate::proto::encode_size_into(*offset, order, &mut b);
            crate::proto::encode_size_into(*count, order, &mut b);
            crate::proto::encode_size_into(*stride, order, &mut b);
            (QosFlags::GET, b)
        }
        ArrayReq::Put {
            offset,
            stride,
            value,
        } => {
            let mut b = Vec::new();
            crate::proto::encode_size_into(*offset, order, &mut b);
            crate::proto::encode_size_into(*stride, order, &mut b);
            encode_pv_field(value, &array_desc, order, &mut b);
            (0x00, b)
        }
        ArrayReq::SetLength { length } => {
            let mut b = Vec::new();
            crate::proto::encode_size_into(*length, order, &mut b);
            (QosFlags::GET_PUT, b)
        }
        ArrayReq::GetLength => (QosFlags::PROCESS, Vec::new()),
    };
    let mut data = Vec::with_capacity(9 + body.len());
    data.put_u32(sid, order);
    data.put_u32(ioid, order);
    data.put_u8(data_subcmd);
    data.extend_from_slice(&body);
    let data_h = PvaHeader::application(false, order, Command::Array.code(), data.len() as u32);
    let mut data_frame = Vec::with_capacity(8 + data.len());
    data_h.write_into(&mut data_frame);
    data_frame.extend_from_slice(&data);
    server.send_for_channel(sid, data_frame).await?;

    let resp_frame = await_frame(&mut stream, op_timeout).await?;
    let result = match decode_array_data(&resp_frame, &req, &array_desc, &mut cache) {
        Ok(Ok(resp)) => Ok(resp),
        Ok(Err(status)) => Err(PvaError::RemoteError(status)),
        Err(e) => {
            server.close();
            Err(e)
        }
    };

    ioid_guard.disarm();
    let destroy = codec.build_destroy_request(sid, ioid);
    let _ = server.send_for_channel(sid, destroy).await;
    server.unregister_ioid(ioid);
    result.map(|resp| (array_desc, resp))
}

/// Decode a ChannelArray INIT reply: `ioid + subcmd + status + array
/// introspection`. Two-level result (see [`decode_put_get_init`]): outer
/// `Err` is connection-fatal, inner `Ok(Err(status))` a per-op failure.
fn decode_array_init(
    frame: &super::decode::Frame,
    type_cache: &mut crate::pvdata::encode::TypeCache,
) -> PvaResult<Result<FieldDesc, crate::proto::Status>> {
    if frame.header.command != Command::Array.code() {
        return Err(PvaError::Protocol(format!(
            "expected ARRAY INIT, got command {}",
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
            "expected ARRAY INIT subcmd, got 0x{subcmd:02x}"
        )));
    }
    let status = crate::proto::Status::decode(&mut cur, order)
        .map_err(|e| PvaError::Decode(e.to_string()))?;
    if !status.is_success() {
        return Ok(Err(status));
    }
    let array_desc = crate::pvdata::encode::decode_type_desc_cached(&mut cur, order, type_cache)
        .map_err(|e| PvaError::Decode(e.to_string()))?;
    Ok(Ok(array_desc))
}

/// Decode a ChannelArray data reply. The trailing body depends on the
/// sub-op: getArray → array value; getLength → size; put/setLength → none.
fn decode_array_data(
    frame: &super::decode::Frame,
    req: &ArrayReq<'_>,
    array_desc: &FieldDesc,
    type_cache: &mut crate::pvdata::encode::TypeCache,
) -> PvaResult<Result<ArrayResp, crate::proto::Status>> {
    if frame.header.command != Command::Array.code() {
        return Err(PvaError::Protocol(format!(
            "expected ARRAY data, got command {}",
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
    let resp = match req {
        ArrayReq::Get { .. } => {
            let value = crate::pvdata::encode::decode_pv_field_cached(
                array_desc, &mut cur, order, type_cache,
            )
            .map_err(|e| PvaError::Decode(e.to_string()))?;
            ArrayResp::Value(value)
        }
        ArrayReq::GetLength => {
            let n = crate::proto::decode_size(&mut cur, order)
                .map_err(|e| PvaError::Decode(e.to_string()))?
                .ok_or_else(|| PvaError::Decode("ARRAY getLength: null size marker".into()))?;
            ArrayResp::Length(n)
        }
        ArrayReq::Put { .. } | ArrayReq::SetLength { .. } => ArrayResp::Empty,
    };
    Ok(Ok(resp))
}

/// ChannelArray `getArray` (cmd 14, `QOS_GET`): read the
/// `[offset, count, stride]` slice of the channel's array field. `count == 0`
/// reads to the end (pvAccessCPP `clientContextImpl.cpp:1669-1704`). Returns
/// the array `(FieldDesc, PvField)`.
pub async fn op_array_get(
    channel: &Arc<Channel>,
    offset: u32,
    count: u32,
    stride: u32,
    op_timeout: Duration,
) -> PvaResult<(FieldDesc, PvField)> {
    let (desc, resp) = op_array_data(
        channel,
        None,
        ArrayReq::Get {
            offset,
            count,
            stride,
        },
        op_timeout,
    )
    .await?;
    match resp {
        ArrayResp::Value(v) => Ok((desc, v)),
        _ => Err(PvaError::Protocol(
            "ARRAY getArray: unexpected reply body".into(),
        )),
    }
}

/// ChannelArray `putArray` (cmd 14, subcmd 0): splice `value` into the
/// channel's array field at `offset` with `stride` (pvAccessCPP
/// `clientContextImpl.cpp:1706-1748`).
pub async fn op_array_put(
    channel: &Arc<Channel>,
    value: &PvField,
    offset: u32,
    stride: u32,
    op_timeout: Duration,
) -> PvaResult<()> {
    op_array_data(
        channel,
        None,
        ArrayReq::Put {
            offset,
            stride,
            value,
        },
        op_timeout,
    )
    .await
    .map(|_| ())
}

/// ChannelArray `setLength` (cmd 14, `QOS_GET_PUT`): resize the channel's
/// array field (pvAccessCPP `clientContextImpl.cpp:1750-1782`).
pub async fn op_array_set_length(
    channel: &Arc<Channel>,
    length: u32,
    op_timeout: Duration,
) -> PvaResult<()> {
    op_array_data(channel, None, ArrayReq::SetLength { length }, op_timeout)
        .await
        .map(|_| ())
}

/// ChannelArray `getLength` (cmd 14, `QOS_PROCESS`): query the current
/// element count of the channel's array field (pvAccessCPP
/// `clientContextImpl.cpp:1784-1810`).
pub async fn op_array_get_length(channel: &Arc<Channel>, op_timeout: Duration) -> PvaResult<u32> {
    let (_desc, resp) = op_array_data(channel, None, ArrayReq::GetLength, op_timeout).await?;
    match resp {
        ArrayResp::Length(n) => Ok(n),
        _ => Err(PvaError::Protocol(
            "ARRAY getLength: unexpected reply body".into(),
        )),
    }
}

// ── ARRAY request-carrying variants (gateway pvRequest forwarding) ──────
//
// A PVA-to-PVA gateway must forward the downstream's ARRAY INIT pvRequest
// to the upstream IOC verbatim (pva2pva `GWChannel::createChannelArray`
// forwards `pvRequest` unchanged, `channel.cpp:227-232`) so the upstream
// resolves the same bound array field the downstream selected. These mirror
// the default-pvRequest functions above but thread the caller's pvRequest
// into the INIT frame via [`op_array_data`].

/// ChannelArray INIT-only probe: open the array op with `pv_request` (or the
/// default `field(value)` when `None`), read back the bound array field's
/// introspection, then DESTROY without a data sub-op. The gateway uses this
/// to answer a downstream ARRAY INIT — pvAccessCPP's INIT reply carries the
/// array introspection (`responseHandlers.cpp:2347-2360`), which the gateway
/// must resolve from the upstream rather than fabricate.
pub async fn op_array_describe(
    channel: &Arc<Channel>,
    pv_request: Option<&PvField>,
    op_timeout: Duration,
) -> PvaResult<FieldDesc> {
    requeue_on_disconnect(channel, op_timeout, |budget| {
        op_array_describe_attempt(channel, pv_request, budget)
    })
    .await
}

/// One attempt of [`op_array_describe`]; [`requeue_on_disconnect`] runs it
/// again from the top when the channel is lost mid-op.
async fn op_array_describe_attempt(
    channel: &Arc<Channel>,
    pv_request: Option<&PvField>,
    op_timeout: Duration,
) -> PvaResult<FieldDesc> {
    let (server, sid) = ensure_active_with_op_timeout(channel, op_timeout).await?;
    let order = server.byte_order();
    let big_endian = matches!(order, ByteOrder::Big);
    let codec = PvaCodec { big_endian };
    let ioid = alloc_ioid();

    let pv_req = match pv_request {
        Some(req) => encode_pv_request_value(req, order),
        None => build_pv_request_value_only(big_endian),
    };
    let mut stream = server.register_ioid_stream(sid, ioid, Command::Array.code());
    let mut ioid_guard = IoidGuard::new(server.clone(), ioid);
    // Op-local type cache (ChannelArray): INIT and DATA run on one task in
    // wire order. The reader does not flatten ARRAY DATA (its layout is
    // sub-op dependent), so this op-local cache resolves its within-op
    // markers.
    let mut cache = crate::pvdata::encode::TypeCache::new();

    let mut init = Vec::with_capacity(9 + pv_req.len());
    init.put_u32(sid, order);
    init.put_u32(ioid, order);
    init.put_u8(QosFlags::INIT);
    init.extend_from_slice(&pv_req);
    let init_h = PvaHeader::application(false, order, Command::Array.code(), init.len() as u32);
    let mut init_frame = Vec::with_capacity(8 + init.len());
    init_h.write_into(&mut init_frame);
    init_frame.extend_from_slice(&init);
    server.send_for_channel(sid, init_frame).await?;

    let init_resp = await_frame(&mut stream, op_timeout).await?;
    let array_desc = match decode_array_init(&init_resp, &mut cache) {
        Ok(Ok(desc)) => desc,
        Ok(Err(status)) => {
            server.unregister_ioid(ioid);
            return Err(PvaError::RemoteError(status));
        }
        Err(e) => {
            server.close();
            return Err(e);
        }
    };
    // INIT succeeded → the op is registered upstream; arm the destroy so the
    // guard tears it down on the (immediate) drop below.
    ioid_guard.arm_destroy(sid);
    ioid_guard.disarm();
    let destroy = codec.build_destroy_request(sid, ioid);
    let _ = server.send_for_channel(sid, destroy).await;
    server.unregister_ioid(ioid);
    Ok(array_desc)
}

/// [`op_array_get`] carrying a preserved INIT `pv_request`.
pub async fn op_array_get_with_request(
    channel: &Arc<Channel>,
    pv_request: &PvField,
    offset: u32,
    count: u32,
    stride: u32,
    op_timeout: Duration,
) -> PvaResult<(FieldDesc, PvField)> {
    let (desc, resp) = op_array_data(
        channel,
        Some(pv_request),
        ArrayReq::Get {
            offset,
            count,
            stride,
        },
        op_timeout,
    )
    .await?;
    match resp {
        ArrayResp::Value(v) => Ok((desc, v)),
        _ => Err(PvaError::Protocol(
            "ARRAY getArray: unexpected reply body".into(),
        )),
    }
}

/// [`op_array_put`] carrying a preserved INIT `pv_request`.
pub async fn op_array_put_with_request(
    channel: &Arc<Channel>,
    pv_request: &PvField,
    value: &PvField,
    offset: u32,
    stride: u32,
    op_timeout: Duration,
) -> PvaResult<()> {
    op_array_data(
        channel,
        Some(pv_request),
        ArrayReq::Put {
            offset,
            stride,
            value,
        },
        op_timeout,
    )
    .await
    .map(|_| ())
}

/// [`op_array_set_length`] carrying a preserved INIT `pv_request`.
pub async fn op_array_set_length_with_request(
    channel: &Arc<Channel>,
    pv_request: &PvField,
    length: u32,
    op_timeout: Duration,
) -> PvaResult<()> {
    op_array_data(
        channel,
        Some(pv_request),
        ArrayReq::SetLength { length },
        op_timeout,
    )
    .await
    .map(|_| ())
}

/// [`op_array_get_length`] carrying a preserved INIT `pv_request`.
pub async fn op_array_get_length_with_request(
    channel: &Arc<Channel>,
    pv_request: &PvField,
    op_timeout: Duration,
) -> PvaResult<u32> {
    let (_desc, resp) =
        op_array_data(channel, Some(pv_request), ArrayReq::GetLength, op_timeout).await?;
    match resp {
        ArrayResp::Length(n) => Ok(n),
        _ => Err(PvaError::Protocol(
            "ARRAY getLength: unexpected reply body".into(),
        )),
    }
}

// ── PROCESS (cmd 16) ────────────────────────────────────────────────────

/// Build a PROCESS INIT frame (`sid + ioid + 0x08 + pvRequest`) carrying
/// the caller-supplied `pv_req` bytes verbatim. Factored out so the
/// caller's request can be verified at the wire level (regression).
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
    requeue_on_disconnect(channel, op_timeout, |budget| {
        op_process_with_request_attempt(channel, pv_req, budget)
    })
    .await
}

/// One attempt of [`op_process_with_request`]; [`requeue_on_disconnect`] runs it
/// again from the top when the channel is lost mid-op.
async fn op_process_with_request_attempt(
    channel: &Arc<Channel>,
    pv_req: &[u8],
    op_timeout: Duration,
) -> PvaResult<()> {
    let (server, sid) = ensure_active_with_op_timeout(channel, op_timeout).await?;
    let order = server.byte_order();
    let big_endian = matches!(order, ByteOrder::Big);
    let codec = PvaCodec { big_endian };
    let ioid = alloc_ioid();

    let mut stream = server.register_ioid_stream(sid, ioid, Command::Process.code());
    let mut ioid_guard = IoidGuard::new(server.clone(), ioid);

    // INIT — `sid + ioid + 0x08 + pvRequest`.
    let init_frame = build_process_init(sid, ioid, pv_req, order);
    server.send_for_channel(sid, init_frame).await?;

    let init_resp = await_frame(&mut stream, op_timeout).await?;
    let init_status = match decode_process_status(&init_resp, true) {
        Ok(s) => s,
        Err(e) => {
            // Decode fault / command mismatch / wrong phase on the INIT
            // reply is fatal.
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
    let result = match decode_process_status(&resp_frame, false) {
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

/// [`op_process_with_request`] variant taking the PROCESS pvRequest as a
/// decoded [`PvField`] value. It is serialized (`type + full value`, as
/// pvxs `serverget.cpp` decodes INIT pvRequests) in the connection's
/// negotiated byte order, so the caller need not pre-encode it in the
/// right endianness. A PVA-to-PVA gateway uses this to forward a
/// downstream PROCESS create-time pvRequest upstream
/// (`ChannelContext.pv_request` → pva2pva createChannelProcess).
pub async fn op_process_with_request_value(
    channel: &Arc<Channel>,
    pv_request: &PvField,
    op_timeout: Duration,
) -> PvaResult<()> {
    requeue_on_disconnect(channel, op_timeout, |budget| {
        op_process_with_request_value_attempt(channel, pv_request, budget)
    })
    .await
}

/// One attempt of [`op_process_with_request_value`]; [`requeue_on_disconnect`] runs it
/// again from the top when the channel is lost mid-op.
async fn op_process_with_request_value_attempt(
    channel: &Arc<Channel>,
    pv_request: &PvField,
    op_timeout: Duration,
) -> PvaResult<()> {
    let (server, sid) = ensure_active_with_op_timeout(channel, op_timeout).await?;
    let order = server.byte_order();
    let big_endian = matches!(order, ByteOrder::Big);
    let codec = PvaCodec { big_endian };
    let ioid = alloc_ioid();

    let mut stream = server.register_ioid_stream(sid, ioid, Command::Process.code());
    let mut ioid_guard = IoidGuard::new(server.clone(), ioid);

    // INIT — `sid + ioid + 0x08 + pvRequest`, the pvRequest encoded
    // (`type + full value`) in the connection's byte order.
    let desc = pv_request.descriptor();
    let mut pv_req = Vec::new();
    encode_type_desc(&desc, order, &mut pv_req);
    encode_pv_field(pv_request, &desc, order, &mut pv_req);
    let init_frame = build_process_init(sid, ioid, &pv_req, order);
    server.send_for_channel(sid, init_frame).await?;

    let init_resp = await_frame(&mut stream, op_timeout).await?;
    let init_status = match decode_process_status(&init_resp, true) {
        Ok(s) => s,
        Err(e) => {
            // Decode fault / command mismatch / wrong phase on the INIT
            // reply is fatal.
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
    let result = match decode_process_status(&resp_frame, false) {
        Ok(s) if s.is_success() => Ok(()),
        Ok(s) => Err(PvaError::Protocol(format!("PROCESS failed: {s:?}"))),
        Err(e) => {
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

/// Decode a `PROCESS` response: `ioid + subcmd + status`, enforcing the
/// expected wire phase. Returns the decoded `Status` for the matching
/// phase.
///
/// `expect_init` selects the phase. pvAccess `BaseRequestImpl::response()`
/// routes purely on `qos & QOS_INIT` (clientContextImpl.cpp:315-342): an
/// INIT-bit reply runs `initResponse()` (channelProcessConnect) and a
/// non-INIT reply runs `normalResponse()` (processDone), and
/// `ChannelProcessRequestImpl::process()` refuses to send the data request
/// until a successful INIT set `m_initialized`. Both PROCESS phases carry
/// an identical status-only body, so the subcmd is the *only* phase
/// discriminator: dropping it let a peer pass off a normal response as the
/// INIT-ack, or an INIT-ack as process completion. The INIT reply must
/// therefore carry `QOS_INIT` and the process-done reply must not.
///
/// An `Err` from this decoder is always connection-fatal — a command
/// mismatch, a wrong-phase subcmd, or a truncated body is a protocol
/// violation that pvxs answers with `bev.reset()` (clientget.cpp:456-493).
/// A non-success `Status` *for the correct phase* decodes to `Ok`: it is a
/// per-operation result, NOT a fault, so the caller surfaces it without
/// resetting the circuit.
fn decode_process_status(
    frame: &super::decode::Frame,
    expect_init: bool,
) -> PvaResult<crate::proto::Status> {
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
    let subcmd = cur.get_u8().map_err(|e| PvaError::Decode(e.to_string()))?;
    let is_init = subcmd & QosFlags::INIT != 0;
    if is_init != expect_init {
        return Err(PvaError::Protocol(format!(
            "PROCESS response phase mismatch: expected {}, got subcmd {subcmd:#04x}",
            if expect_init { "INIT" } else { "process-done" },
        )));
    }
    let status = crate::proto::Status::decode(&mut cur, order)
        .map_err(|e| PvaError::Decode(e.to_string()))?;
    Ok(status)
}

// ── Helpers ────────────────────────────────────────────────────────────

/// Await one frame for an op's IOID.
///
/// A closed router slot means this op lost its transport, and that is
/// [`PvaError::Disconnected`] — pvxs's `Disconnect` exception — not a
/// protocol error. The two producers are `Channel::disconnect`
/// (`src/client.cpp:198-204`), which drops the IOID maps on a server-initiated
/// `CMD_DESTROY_CHANNEL`, and the reader task's exit. The DESTROY case
/// leaves the TCP circuit up and still serving this connection's other
/// channels, so the old "connection closed" text was wrong on its face.
/// [`requeue_on_disconnect`] is what acts on this variant.
async fn await_frame(
    stream: &mut mpsc::UnboundedReceiver<super::decode::Frame>,
    op_timeout: Duration,
) -> PvaResult<super::decode::Frame> {
    let frame = timeout(op_timeout, stream.recv())
        .await
        .map_err(|_| PvaError::Timeout)?
        .ok_or(PvaError::Disconnected)?;
    Ok(frame)
}

/// Outcome of [`recv_monitor_init`].
enum MonitorInit {
    /// The server's MONITOR INIT reply arrived.
    Reply(super::decode::Frame),
    /// A stop()/teardown fired (or `stop` was already set) before the
    /// reply: the caller must unregister the IOID and end ChannelClosed.
    Cancelled,
    /// The frame stream closed before any reply: connection lost.
    Lost,
}

/// Race a monitor's INIT reply against the subscription's cancel signal.
///
/// This is the await that sits after `register_ioid_monitor` but before
/// `active` is published. A stop()/teardown issued in that window must
/// complete the caller's cancel promptly instead of parking forever on a
/// silent or withholding server — the same hazard the data loops guard.
/// pvxs `_cancel()` completes synchronously even in the Creating phase and
/// sends no DESTROY for a not-yet-acknowledged op (clientmon.cpp:810-824),
/// so the caller's [`MonitorInit::Cancelled`] handling unregisters only the
/// local IOID. With no handle (`state` is `None`) the op cannot be
/// cancelled, so it simply awaits the reply. Shared by `run_monitor_loop`
/// and `run_raw_monitor_loop` so both honour the same rule.
async fn recv_monitor_init(
    state: &Option<Arc<SubscriptionState>>,
    stream: &super::monitor_queue::MonitorBacklog,
) -> MonitorInit {
    match state {
        Some(s) => {
            // A teardown that raced just ahead of this await already set
            // `stop`; honour it before parking on the stream.
            if s.stop.load(Ordering::Relaxed) {
                return MonitorInit::Cancelled;
            }
            tokio::select! {
                biased;
                _ = s.cancel.notified() => MonitorInit::Cancelled,
                f = stream.recv() => match f {
                    Some(f) => MonitorInit::Reply(f),
                    None => MonitorInit::Lost,
                },
            }
        }
        None => match stream.recv().await {
            Some(f) => MonitorInit::Reply(f),
            None => MonitorInit::Lost,
        },
    }
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
        .map_err(|_| PvaError::Disconnected)
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

/// The descriptor a bare `.value` write targets: the `value` member of
/// a structure prototype, or the prototype itself when it is already a
/// bare leaf. Used to classify legacy positional PUT tokens against the
/// shape the server actually exposes.
fn value_target_desc(intro: &FieldDesc) -> &FieldDesc {
    if let FieldDesc::Structure { fields, .. } = intro {
        if let Some((_, child)) = fields.iter().find(|(n, _)| n == "value") {
            return child;
        }
    }
    intro
}

/// Classify the legacy pvAccessCPP positional bare-token PUT form
/// against the prototype and lower it to a full [`PvField`] via
/// [`build_put_value`]. pvAccessCPP `pvtoolsSrc/pvput.cpp:144-178`:
///
/// - scalar-array `.value`: a lone `[...]` token is the JSON-array
///   shortcut (`value=[...]`); otherwise the first token is the
///   compatibility length and is dropped (`bare.slice(1)`), the rest
///   are the elements.
/// - scalar `.value`: exactly one token (more than one is rejected,
///   matching upstream's "Can't assign multiple values to scalar").
/// - any other shape: tokens are space-joined and lowered as-is, so
///   structure / union / variant prototypes behave as before.
///
/// The decision needs the prototype, so this runs from inside the PUT
/// op after INIT (see [`op_put_inner_build`]).
fn build_put_value_from_tokens(intro: &FieldDesc, tokens: &[String]) -> PvaResult<PvField> {
    match value_target_desc(intro) {
        FieldDesc::ScalarArray(_) => {
            // `build_put_value`'s scalar-array arm splits on commas, so
            // lower the chosen element set to a comma-separated string.
            let csv = if tokens.len() == 1 && tokens[0].trim_start().starts_with('[') {
                // JSON-array shortcut: `value=[...]`. Strip the outer
                // brackets; the inner is already comma-separated scalars.
                let t = tokens[0].trim();
                let inner = t
                    .strip_prefix('[')
                    .and_then(|s| s.strip_suffix(']'))
                    .unwrap_or(t);
                inner.to_string()
            } else {
                // Legacy positional: first token is the length, ignored.
                tokens
                    .iter()
                    .skip(1)
                    .map(String::as_str)
                    .collect::<Vec<_>>()
                    .join(",")
            };
            build_put_value(intro, &csv)
        }
        FieldDesc::Scalar(_) => {
            if tokens.len() != 1 {
                return Err(PvaError::InvalidValue(
                    "Can't assign multiple values to scalar".to_string(),
                ));
            }
            build_put_value(intro, &tokens[0])
        }
        _ => build_put_value(intro, &tokens.join(" ")),
    }
}

/// The changed-bitset for a bare `.value` write: the `value` field's
/// bit, or the root bit (0) when the prototype is itself a bare leaf
/// with no `value` member.
fn value_only_bit_set(intro: &FieldDesc) -> BitSet {
    let mut changed = BitSet::new();
    if let Some(bit) = intro.bit_for_path("value") {
        changed.set(bit);
    } else {
        changed.set(0);
    }
    changed
}

/// True when the prototype's writable `.value` is an `enum_t` structure
/// (NTEnum) — the case pvput handles by writing `value.index` rather than
/// the whole `.value` (pvput.cpp:180). The `index` member is required so a
/// degenerate `enum_t`-named structure without it is not mistaken for one.
fn value_target_is_enum(intro: &FieldDesc) -> bool {
    matches!(
        value_target_desc(intro),
        FieldDesc::Structure { struct_id, fields }
            if struct_id == "enum_t" && fields.iter().any(|(n, _)| n == "index")
    )
}

/// The `value.choices` labels from a previously-fetched value snapshot, used
/// to resolve an enum write by label. `None` when the snapshot is absent or
/// does not carry a `value.choices` string array.
///
/// A decoded wire value carries a string array as the canonical typed form
/// [`PvField::ScalarArrayTyped`] — both the GET snapshot path and the in-PUT
/// `GetOPut` path go through `decode_pv_field_with_bitset_cached`, which
/// emits `ScalarArrayTyped` for `string[]`. Locally-built values may instead
/// use the untyped [`PvField::ScalarArray`]. Resolving the label must accept
/// either, else an enum-by-label put silently falls through to the integer
/// fallback and rejects every real label.
fn enum_choices_from_previous(previous: &PvField) -> Option<Vec<String>> {
    let labels = |items: &[ScalarValue]| items.iter().map(|sv| sv.to_string()).collect::<Vec<_>>();
    match value_at_path(previous, &["value", "choices"])? {
        PvField::ScalarArray(items) => Some(labels(&items)),
        PvField::ScalarArrayTyped(a) => Some(labels(&a.to_scalar_values())),
        _ => None,
    }
}

/// Resolve a bare enum token to its `index`, matching the current choice
/// labels first and falling back to an integer index — pvput.cpp:190-202:
/// a token equal to a choice writes that choice's position, otherwise it is
/// parsed as the integer index directly. `choices` is `None` when no
/// get-first snapshot is available (the integer fallback still applies).
fn resolve_enum_index(token: &str, choices: Option<&[String]>) -> PvaResult<i32> {
    if let Some(choices) = choices {
        if let Some(i) = choices.iter().position(|c| c == token) {
            return Ok(i as i32);
        }
    }
    token.trim().parse::<i32>().map_err(|_| {
        PvaError::InvalidValue(format!(
            "enum value '{token}' is neither a choice label nor an integer index"
        ))
    })
}

/// Build the `(value, changed-bits)` delta for a bare write to an `enum_t`
/// `.value`: set `value.index` to the resolved index and mark ONLY that bit
/// (pvput.cpp:180-204, `args.tosend.set(idxfld->getFieldOffset())`). Leaving
/// `value.choices` unmarked means the server keeps the menu it already
/// holds — marking the whole `.value` would clobber `choices` with an empty
/// array.
fn build_enum_value_delta(intro: &FieldDesc, index: i32) -> PvaResult<(PvField, BitSet)> {
    let mut value = crate::pvdata::encode::default_value_for(intro);
    assign_at_path(
        &mut value,
        &["value", "index"],
        PvField::Scalar(ScalarValue::Int(index)),
    );
    let bit = intro.bit_for_path("value.index").ok_or_else(|| {
        PvaError::InvalidValue("enum prototype has no value.index field".to_string())
    })?;
    let mut changed = BitSet::new();
    changed.set(bit);
    Ok((value, changed))
}

/// Plain-value (bare token) PUT mode against the `.value` member, the single
/// owner of pvput.cpp:155-207. An `enum_t` `.value` resolves the lone token
/// to `value.index` (by choice label, then integer) using the optional
/// get-first snapshot `previous`, marking only `value.index`; every other
/// shape lowers through [`build_put_value_from_tokens`] and marks the
/// `.value` bit. Shared by the deferred CLI classifier
/// ([`build_put_from_args`]) and the legacy positional token form
/// ([`op_put_tokens`]).
fn build_put_value_mode(
    intro: &FieldDesc,
    tokens: &[String],
    previous: Option<&PvField>,
) -> PvaResult<(PvField, BitSet)> {
    if value_target_is_enum(intro) {
        if tokens.len() != 1 {
            return Err(PvaError::InvalidValue(
                "Can't assign multiple values to enum".to_string(),
            ));
        }
        let choices = previous.and_then(enum_choices_from_previous);
        let index = resolve_enum_index(&tokens[0], choices.as_deref())?;
        return build_enum_value_delta(intro, index);
    }
    Ok((
        build_put_value_from_tokens(intro, tokens)?,
        value_only_bit_set(intro),
    ))
}

/// One leaf of a multi-field PUT assignment. The leaf kind is explicit
/// so the single delta owner (`build_field_delta`) places each value
/// the right way: a [`PutLeaf::Str`] is parsed against the target
/// descriptor by the CLI parser, while a [`PutLeaf::Typed`] is assigned
/// as pvData with no `Display`/parse round trip — so a typed scalar
/// array travels as its original payload instead of a bracketed string.
///
/// pvxs `linkBuildPut` stages each OUT link's DBR payload typed and
/// moves it into the selected field directly (`pvxs/ioc/pvalink_channel.cpp:127`);
/// [`PutLeaf::Typed`] is the client-side primitive for that combined
/// sibling-field PUT path.
pub enum PutLeaf {
    /// CLI/text leaf, parsed against the target descriptor by
    /// `build_put_value_for_path`.
    Str(String),
    /// Already-typed pvData leaf, assigned directly into the selected
    /// descriptor leaf.
    Typed(PvField),
}

/// Build a `(value, changed-bits)` delta from explicit dotted-path
/// assignments against the prototype: start from the prototype default,
/// overwrite each assigned leaf, and mark exactly the assigned bits.
/// Single owner of "assignments → wire delta", shared by the text
/// multi-field PUT ([`op_put_fields`]), the typed multi-field PUT
/// ([`op_put_fields_typed`]), and the deferred CLI token classifier
/// ([`build_put_from_args`]). The [`PutLeaf`] kind decides whether a leaf
/// is parsed from text or assigned as already-typed pvData.
fn build_field_delta(
    intro: &FieldDesc,
    assignments: &[(String, PutLeaf)],
) -> PvaResult<(PvField, BitSet)> {
    let mut value = crate::pvdata::encode::default_value_for(intro);
    let mut changed = BitSet::new();
    for (path, leaf) in assignments {
        let parts: Vec<&str> = path.split('.').filter(|s| !s.is_empty()).collect();
        let bit = intro.bit_for_path(path).ok_or_else(|| {
            PvaError::InvalidValue(format!("field path '{path}' not present in introspection"))
        })?;
        match leaf {
            // Text leaf: reuse the single-path builder (parse + tree
            // shape), then lift just this path's leaf into the shared
            // accumulator.
            PutLeaf::Str(value_str) => {
                let one = build_put_value_for_path(intro, &parts, value_str)?;
                let leaf_val = value_at_path(&one, &parts).ok_or_else(|| {
                    PvaError::InvalidValue(format!("could not build field '{path}'"))
                })?;
                assign_at_path(&mut value, &parts, leaf_val);
            }
            // Typed leaf: assign the original pvData payload directly,
            // bypassing `Display`/`ScalarValue::parse` so a typed array
            // keeps its element type and byte content (pvxs `linkBuildPut`
            // `value = tosend`, pvxs/ioc/pvalink_channel.cpp:147-159).
            PutLeaf::Typed(pv) => {
                assign_at_path(&mut value, &parts, pv.clone());
            }
        }
        changed.set(bit);
    }
    Ok((value, changed))
}

/// The descriptor at a dotted field path inside `desc`, or `None` when a
/// segment is missing or a non-structure is traversed. The descriptor
/// twin of [`value_at_path`], used to lower a JSON field value against the
/// right target shape.
fn field_desc_at_path<'a>(desc: &'a FieldDesc, parts: &[&str]) -> Option<&'a FieldDesc> {
    match parts.split_first() {
        None => Some(desc),
        Some((head, tail)) => {
            if let FieldDesc::Structure { fields, .. } = desc {
                let child = fields.iter().find(|(n, _)| n == head).map(|(_, d)| d)?;
                field_desc_at_path(child, tail)
            } else {
                None
            }
        }
    }
}

/// Coerce one JSON scalar to a [`ScalarValue`] of the target type, the
/// pvData `PVScalar::putFrom` coercion the JSON parser applies per token
/// (parseinto.cpp:60-66, `valueAssign` → `castUnsafe`): a JSON number
/// lands in any numeric type (float→int truncates, like a C++ static
/// cast), a JSON bool maps to 0/1 for numerics, and a JSON string is
/// parsed for a numeric/bool target or taken verbatim for a string.
fn json_scalar_to_value(st: ScalarType, j: &serde_json::Value) -> PvaResult<ScalarValue> {
    use ScalarType as T;
    use serde_json::Value as J;
    // Any JSON scalar as a wide integer (i128 holds every i64/u64 exactly;
    // a float source truncates toward zero, matching `static_cast`).
    let as_int = || -> Option<i128> {
        match j {
            J::Number(n) => n
                .as_i64()
                .map(|v| v as i128)
                .or_else(|| n.as_u64().map(|v| v as i128))
                .or_else(|| n.as_f64().map(|f| f as i128)),
            J::Bool(b) => Some(*b as i128),
            J::String(s) => s.trim().parse::<i128>().ok(),
            _ => None,
        }
    };
    let as_float = || -> Option<f64> {
        match j {
            J::Number(n) => n.as_f64(),
            J::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
            J::String(s) => s.trim().parse::<f64>().ok(),
            _ => None,
        }
    };
    let int_err = || PvaError::InvalidValue(format!("cannot assign JSON {j} to an integer field"));
    let float_err =
        || PvaError::InvalidValue(format!("cannot assign JSON {j} to a floating field"));
    Ok(match st {
        T::Boolean => match j {
            J::Bool(b) => ScalarValue::Boolean(*b),
            J::Number(n) => ScalarValue::Boolean(n.as_f64().map(|f| f != 0.0).unwrap_or(false)),
            J::String(s) => ScalarValue::parse(st, s).map_err(PvaError::InvalidValue)?,
            _ => {
                return Err(PvaError::InvalidValue(format!(
                    "cannot assign JSON {j} to a bool field"
                )));
            }
        },
        T::Byte => ScalarValue::Byte(as_int().ok_or_else(int_err)? as i8),
        T::Short => ScalarValue::Short(as_int().ok_or_else(int_err)? as i16),
        T::Int => ScalarValue::Int(as_int().ok_or_else(int_err)? as i32),
        T::Long => ScalarValue::Long(as_int().ok_or_else(int_err)? as i64),
        T::UByte => ScalarValue::UByte(as_int().ok_or_else(int_err)? as u8),
        T::UShort => ScalarValue::UShort(as_int().ok_or_else(int_err)? as u16),
        T::UInt => ScalarValue::UInt(as_int().ok_or_else(int_err)? as u32),
        T::ULong => ScalarValue::ULong(as_int().ok_or_else(int_err)? as u64),
        T::Float => ScalarValue::Float(as_float().ok_or_else(float_err)? as f32),
        T::Double => ScalarValue::Double(as_float().ok_or_else(float_err)?),
        T::String => match j {
            J::String(s) => ScalarValue::String(s.clone().into()),
            J::Number(n) => ScalarValue::String(n.to_string().into()),
            J::Bool(b) => ScalarValue::String(b.to_string().into()),
            _ => {
                return Err(PvaError::InvalidValue(format!(
                    "cannot assign JSON {j} to a string field"
                )));
            }
        },
    })
}

/// Infer an `any` (variant) value from a JSON scalar — narrowest type that
/// holds it, mirroring pvData's variant-union assignment which wraps the
/// JSON token's native scalar type (parseinto.cpp:99-104).
fn json_to_variant(j: &serde_json::Value) -> PvaResult<(FieldDesc, PvField)> {
    use serde_json::Value as J;
    let (st, sv) = match j {
        J::Bool(b) => (ScalarType::Boolean, ScalarValue::Boolean(*b)),
        J::Number(n) if n.is_i64() => (ScalarType::Long, ScalarValue::Long(n.as_i64().unwrap())),
        J::Number(n) if n.is_u64() => (ScalarType::ULong, ScalarValue::ULong(n.as_u64().unwrap())),
        J::Number(n) => (
            ScalarType::Double,
            ScalarValue::Double(n.as_f64().unwrap_or(0.0)),
        ),
        J::String(s) => (ScalarType::String, ScalarValue::String(s.clone().into())),
        _ => {
            return Err(PvaError::InvalidValue(
                "cannot infer an 'any' value from a JSON array, object, or null".to_string(),
            ));
        }
    };
    Ok((FieldDesc::Scalar(st), PvField::Scalar(sv)))
}

/// Lower a parsed JSON value into a [`PvField`] matching `desc`, marking
/// each assigned leaf in `changed` at its depth-first bit offset
/// (`base_bit` is `desc`'s own offset). This is the descriptor-driven
/// counterpart of pvData `parseJSON(strm, dest, assigned)`
/// (parseinto.cpp): a structure assigns only its named JSON keys and marks
/// each assigned leaf (not the container — so unmentioned siblings keep
/// their server value); a scalar / scalar-array / array element marks its
/// own bit. An unknown JSON key is an error, like pvData's
/// `getSubFieldT` (parseinto.cpp:212). Union targets are rejected (pvData's
/// member auto-select is ambiguous from a bare CLI token).
fn parse_json_into_field(
    desc: &FieldDesc,
    j: &serde_json::Value,
    base_bit: usize,
    changed: &mut BitSet,
) -> PvaResult<PvField> {
    match desc {
        FieldDesc::Scalar(st) => {
            let v = json_scalar_to_value(*st, j)?;
            changed.set(base_bit);
            Ok(PvField::Scalar(v))
        }
        FieldDesc::ScalarArray(st) => {
            let arr = j.as_array().ok_or_else(|| {
                PvaError::InvalidValue(format!(
                    "expected a JSON array for a scalar-array field, got {j}"
                ))
            })?;
            let mut items = Vec::with_capacity(arr.len());
            for e in arr {
                items.push(json_scalar_to_value(*st, e)?);
            }
            changed.set(base_bit);
            Ok(PvField::ScalarArray(items))
        }
        FieldDesc::Structure { struct_id, fields } => {
            let obj = j.as_object().ok_or_else(|| {
                PvaError::InvalidValue(format!(
                    "expected a JSON object for a structure field, got {j}"
                ))
            })?;
            // pvData `getSubFieldT` throws on an unknown key (parseinto.cpp:212).
            for k in obj.keys() {
                if !fields.iter().any(|(n, _)| n == k) {
                    return Err(PvaError::InvalidValue(format!(
                        "JSON field '{k}' not present in target structure"
                    )));
                }
            }
            let mut s = PvStructure::new(struct_id);
            let mut child_bit = base_bit + 1;
            for (name, child) in fields {
                let cv = match obj.get(name) {
                    Some(jv) => parse_json_into_field(child, jv, child_bit, changed)?,
                    None => crate::pvdata::encode::default_value_for(child),
                };
                s.fields.push((name.clone(), cv));
                child_bit += child.total_bits();
            }
            // The container's own bit stays clear — only assigned leaves are
            // marked, so unmentioned siblings keep their server-side value.
            Ok(PvField::Structure(s))
        }
        FieldDesc::StructureArray { struct_id, fields } => {
            let arr = j.as_array().ok_or_else(|| {
                PvaError::InvalidValue(format!(
                    "expected a JSON array for a structure-array field, got {j}"
                ))
            })?;
            let elem_desc = FieldDesc::Structure {
                struct_id: struct_id.clone(),
                fields: fields.clone(),
            };
            let mut items = Vec::with_capacity(arr.len());
            for e in arr {
                // Element-internal bits are not part of the top BitSet — pvData
                // pushes structureArray-element frames with a null `assigned`
                // and marks only the array field's own offset
                // (parseinto.cpp:191,267). A scratch set absorbs them.
                let mut scratch = BitSet::new();
                match parse_json_into_field(&elem_desc, e, 0, &mut scratch)? {
                    PvField::Structure(st) => items.push(Some(st)),
                    other => {
                        return Err(PvaError::InvalidValue(format!(
                            "internal: structure-array element built as {other:?}"
                        )));
                    }
                }
            }
            changed.set(base_bit);
            Ok(PvField::StructureArray(items))
        }
        FieldDesc::Variant => {
            let (vdesc, vval) = json_to_variant(j)?;
            changed.set(base_bit);
            Ok(PvField::Variant(Box::new(VariantValue {
                desc: Some(vdesc),
                value: vval,
            })))
        }
        FieldDesc::VariantArray => {
            let arr = j.as_array().ok_or_else(|| {
                PvaError::InvalidValue(format!("expected a JSON array for an any[] field, got {j}"))
            })?;
            let mut items = Vec::with_capacity(arr.len());
            for e in arr {
                let (vd, vv) = json_to_variant(e)?;
                items.push(Some(VariantValue {
                    desc: Some(vd),
                    value: vv,
                }));
            }
            changed.set(base_bit);
            Ok(PvField::VariantArray(items))
        }
        FieldDesc::Union { .. } | FieldDesc::UnionArray { .. } => Err(PvaError::InvalidValue(
            "JSON assignment to a union field is not supported".to_string(),
        )),
    }
}

/// Field=value mode lowering (pvput.cpp:209-244): a pair whose value starts
/// with `[` or `{` is JSON (an array lowers into a scalar/struct array, an
/// object into a (sub)structure — pvData `parseJSON`/`jarray`), marking the
/// assigned leaves; every other pair is a scalar text assignment marking
/// the field's own bit. Single owner of the CLI field-mode delta, kept
/// distinct from the programmatic [`build_field_delta`] (pvxs
/// `PutBuilder::set`) so the pvput-only JSON lowering does not leak into
/// the typed-setter path.
/// Parse one piece of `pvput` JSON input the way C parses it: in yajl's
/// JSON5 dialect, not in strict JSON.
///
/// `pvput` hands its input to `parseJSON` (`pvAccess/pvtoolsSrc/pvput.cpp:150-153`),
/// which drives a handle from `yajl_alloc`, and that handle's flags are
/// already `yajl_allow_json5 | yajl_allow_comments`
/// (`libcom/src/yajl/yajl.c:77`). Nothing on the path clears them —
/// `pvData/src/json/parseinto.cpp:315-345` only re-enables comments. So
/// `{value:1}`, a `// comment` or a single-quoted string are input C
/// accepts, and `serde_json` alone rejected every one of them. The
/// conversion is `epics_base_rs::json5`, the workspace's single reader of
/// that dialect.
fn parse_pvput_json(raw: &str) -> Result<serde_json::Value, String> {
    let strict = epics_base_rs::json5::relaxed_to_strict(raw)
        .map_err(|e| format!("invalid JSON value: {e}"))?;
    serde_json::from_str(&strict).map_err(|e| format!("invalid JSON value: {e}"))
}

fn build_put_field_pairs(
    intro: &FieldDesc,
    pairs: &[(String, String)],
) -> PvaResult<(PvField, BitSet)> {
    let mut value = crate::pvdata::encode::default_value_for(intro);
    let mut changed = BitSet::new();
    for (fname, raw) in pairs {
        let parts: Vec<&str> = fname.split('.').filter(|s| !s.is_empty()).collect();
        let bit = intro.bit_for_path(fname).ok_or_else(|| {
            PvaError::InvalidValue(format!("field path '{fname}' not present in introspection"))
        })?;
        let trimmed = raw.trim_start();
        if trimmed.starts_with('[') || trimmed.starts_with('{') {
            let field_desc = field_desc_at_path(intro, &parts).ok_or_else(|| {
                PvaError::InvalidValue(format!("field path '{fname}' not present in introspection"))
            })?;
            let json = parse_pvput_json(raw.trim())
                .map_err(|e| PvaError::InvalidValue(format!("{fname} : {e}")))?;
            // `parse_json_into_field` marks the assigned leaves (or the
            // array/leaf bit) — do not also blanket-mark the field bit.
            let fv = parse_json_into_field(field_desc, &json, bit, &mut changed)?;
            assign_at_path(&mut value, &parts, fv);
        } else {
            let one = build_put_value_for_path(intro, &parts, raw)?;
            let leaf_val = value_at_path(&one, &parts).ok_or_else(|| {
                PvaError::InvalidValue(format!("could not build field '{fname}'"))
            })?;
            assign_at_path(&mut value, &parts, leaf_val);
            changed.set(bit);
        }
    }
    Ok((value, changed))
}

/// Classify the raw CLI PUT tokens against the server prototype and
/// build the `(value, changed-bits)` delta — deferring every
/// field-vs-bare decision until the prototype is known, exactly like
/// pvAccessCPP `pvtoolsSrc/pvput.cpp:109-235`. The pre-fix CLI guessed
/// at parse time (any `=` token forced field mode, mixed input was
/// rejected before contacting the server); this runs from inside the
/// PUT op after INIT, where `intro` is the real structure.
///
/// Rules (pvput.cpp:109-148):
/// - a token without `=` is a bare value;
/// - `f=v` where `f` resolves in the prototype is a field assignment;
/// - `f=v` where `f` does NOT resolve is a bare value carrying a literal
///   `=` IF the prototype's `.value` is a `string` scalar (pvput.cpp:123-130),
///   otherwise a warning is printed and the token is ignored
///   (pvput.cpp:131-134);
/// - bare values and field pairs both present → "Can't mix" (pvput.cpp:139-140);
/// - everything ignored → "No valid value(s)" (pvput.cpp:141-143).
///
/// A lone `[...]` is the `value=[...]` shorthand and a lone `{...}` is JSON
/// top mode (the whole object parsed into the root structure with
/// leaf-level marking); every other bare value lowers through
/// [`build_put_value_from_tokens`] (legacy positional array length-drop)
/// and field pairs lower through [`build_put_field_pairs`] (JSON-aware).
fn build_put_from_args(
    intro: &FieldDesc,
    tokens: &[String],
    previous: Option<&PvField>,
) -> PvaResult<(PvField, BitSet)> {
    // pvput.cpp:124-130: an unresolved `f=v` is a bare string value only
    // when the writable `.value` target is a `string` scalar.
    let value_is_string = matches!(
        value_target_desc(intro),
        FieldDesc::Scalar(crate::pvdata::ScalarType::String)
    );

    let mut bare: Vec<String> = Vec::new();
    let mut pairs: Vec<(String, String)> = Vec::new();
    for tok in tokens {
        match tok.split_once('=') {
            // No `=`: a plain bare value (pvput.cpp:112-114).
            None => bare.push(tok.clone()),
            // `f=v` with `f` present in the prototype: a field assignment
            // (pvput.cpp:116-121).
            Some((fname, val)) if !fname.is_empty() && intro.bit_for_path(fname).is_some() => {
                pairs.push((fname.to_string(), val.to_string()));
            }
            // `f=v` with `f` absent (or an empty field name): either a
            // bare string value containing `=`, or warn-and-ignore.
            Some((fname, _)) => {
                if value_is_string {
                    bare.push(tok.clone());
                } else {
                    eprintln!("{fname} : Warning: no such field. Ignoring it.");
                }
            }
        }
    }

    if !bare.is_empty() && !pairs.is_empty() {
        return Err(PvaError::InvalidValue(
            "Can't mix bare values and field=value pairs".to_string(),
        ));
    }
    if bare.is_empty() && pairs.is_empty() {
        return Err(PvaError::InvalidValue(
            "No valid value(s) specified".to_string(),
        ));
    }

    // pvput.cpp:144-148: a lone `[...]` is shorthand for `value=[...]`, so it
    // lowers through the same JSON-array field path. Only when the prototype
    // exposes a `.value` member — a bare-leaf prototype keeps the legacy
    // positional handling in `build_put_value_from_tokens`, where pvput's
    // root is always a structure.
    if bare.len() == 1
        && bare[0].trim_start().starts_with('[')
        && intro.bit_for_path("value").is_some()
    {
        pairs.push(("value".to_string(), bare.remove(0)));
    }

    // pvput.cpp:150-153: a lone `{...}` is JSON top mode — parse the whole
    // object into the root structure, marking each assigned leaf
    // (`parseJSON(strm, root, &args.tosend)`). pvput's root is always a
    // structure, so this only applies to a structure prototype.
    if bare.len() == 1
        && bare[0].trim_start().starts_with('{')
        && matches!(intro, FieldDesc::Structure { .. })
    {
        let json = parse_pvput_json(bare[0].trim()).map_err(PvaError::InvalidValue)?;
        let mut changed = BitSet::new();
        let value = parse_json_into_field(intro, &json, 0, &mut changed)?;
        return Ok((value, changed));
    }

    if pairs.is_empty() {
        // Plain value mode → `.value` (pvput.cpp:155-207), enum-aware: an
        // `enum_t` `.value` resolves the token to `value.index` against the
        // get-first `previous` snapshot.
        build_put_value_mode(intro, &bare, previous)
    } else {
        // Field=value mode (pvput.cpp:209-244), JSON-aware.
        build_put_field_pairs(intro, &pairs)
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
        (ScalarType::String, ScalarValue::String(trimmed.into()))
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

    /// The PROCESS INIT frame must carry the
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

    /// Monitor `-F delta` must mark only the leaves the server flagged
    /// changed in *this* update. `changed_bitset_to_marked_paths` resolves
    /// the decoded changed `BitSet` to the dotted leaf paths
    /// `format::format_value`'s `marked` argument consumes, with pvData
    /// BitSet-compression propagation (a set structure bit marks its whole
    /// subtree) — pvxs `Value::imarked()` (datafmt.cpp:112-120).
    mod monitor_delta_marks {
        use super::*;

        // NTScalar-shaped prototype: value + alarm{severity,status,message}
        // + timeStamp{secondsPastEpoch,nanoseconds,userTag}. Depth-first
        // bits: 0 root, 1 value, 2 alarm, 3 severity, 4 status, 5 message,
        // 6 timeStamp, 7 secondsPastEpoch, 8 nanoseconds, 9 userTag.
        fn nt_scalar() -> FieldDesc {
            let sub = |fields: Vec<(&str, FieldDesc)>, id: &str| FieldDesc::Structure {
                struct_id: id.to_string(),
                fields: fields
                    .into_iter()
                    .map(|(n, d)| (n.to_string(), d))
                    .collect(),
            };
            sub(
                vec![
                    ("value", FieldDesc::Scalar(ScalarType::Double)),
                    (
                        "alarm",
                        sub(
                            vec![
                                ("severity", FieldDesc::Scalar(ScalarType::Int)),
                                ("status", FieldDesc::Scalar(ScalarType::Int)),
                                ("message", FieldDesc::Scalar(ScalarType::String)),
                            ],
                            "alarm_t",
                        ),
                    ),
                    (
                        "timeStamp",
                        sub(
                            vec![
                                ("secondsPastEpoch", FieldDesc::Scalar(ScalarType::Long)),
                                ("nanoseconds", FieldDesc::Scalar(ScalarType::Int)),
                                ("userTag", FieldDesc::Scalar(ScalarType::Int)),
                            ],
                            "time_t",
                        ),
                    ),
                ],
                "epics:nt/NTScalar:1.0",
            )
        }

        fn marks(bits: &[usize]) -> std::collections::HashSet<String> {
            let mut bs = BitSet::new();
            for &b in bits {
                bs.set(b);
            }
            changed_bitset_to_marked_paths(&nt_scalar(), &bs)
        }

        fn set_of(paths: &[&str]) -> std::collections::HashSet<String> {
            paths.iter().map(|s| s.to_string()).collect()
        }

        /// A value-only update (bit 1) marks exactly `value` — not the
        /// untouched alarm/timeStamp leaves. This is the reprint bug the
        /// fix closes.
        #[test]
        fn value_only_marks_only_value() {
            assert_eq!(marks(&[1]), set_of(&["value"]));
        }

        /// A single nested leaf (bit 3 = alarm.severity) marks only that
        /// leaf.
        #[test]
        fn single_nested_leaf() {
            assert_eq!(marks(&[3]), set_of(&["alarm.severity"]));
        }

        /// A set structure bit (bit 2 = the `alarm` sub-struct) marks every
        /// descendant leaf (BitSet compression), not the struct itself.
        #[test]
        fn struct_bit_marks_whole_subtree() {
            assert_eq!(
                marks(&[2]),
                set_of(&["alarm.severity", "alarm.status", "alarm.message"])
            );
        }

        /// The root bit (bit 0) marks every leaf — the first-snapshot /
        /// full-value case (here surfaced explicitly; the live loop sends
        /// `marked=None` on the first update instead).
        #[test]
        fn root_bit_marks_all_leaves() {
            assert_eq!(
                marks(&[0]),
                set_of(&[
                    "value",
                    "alarm.severity",
                    "alarm.status",
                    "alarm.message",
                    "timeStamp.secondsPastEpoch",
                    "timeStamp.nanoseconds",
                    "timeStamp.userTag",
                ])
            );
        }

        /// A multi-leaf update (value + timeStamp.userTag) marks exactly
        /// those two and nothing else.
        #[test]
        fn disjoint_leaves() {
            assert_eq!(marks(&[1, 9]), set_of(&["value", "timeStamp.userTag"]));
        }

        /// An empty changed set marks nothing.
        #[test]
        fn empty_marks_nothing() {
            assert!(marks(&[]).is_empty());
        }

        /// The resolved paths match `format::format_value`'s `marked`
        /// contract: feeding the set into the Delta formatter prints only
        /// the marked leaves and omits the unmarked ones.
        #[test]
        fn marked_paths_drive_delta_formatter() {
            let desc = nt_scalar();
            // Full value so every leaf is present to (potentially) print.
            let value = crate::pvdata::encode::default_value_for(&desc);
            let mut bs = BitSet::new();
            bs.set(1); // value only
            let marked = changed_bitset_to_marked_paths(&desc, &bs);
            let fmt = crate::format::ValueFmt {
                format: crate::format::ValueFormat::Delta,
                array_limit: 0,
                show_value: true,
            };
            let out = crate::format::format_value(&desc, Some(&value), &fmt, Some(&marked), 0);
            assert!(out.contains("value"), "value leaf must print: {out:?}");
            assert!(
                !out.contains("severity"),
                "unmarked alarm.severity must not print: {out:?}"
            );
            assert!(
                !out.contains("secondsPastEpoch"),
                "unmarked timeStamp leaf must not print: {out:?}"
            );
        }
    }

    /// `pvput`'s JSON input is yajl's JSON5 dialect, not strict JSON:
    /// `parseJSON` (`pvput.cpp:150-153`) drives a `yajl_alloc` handle whose
    /// flags are `yajl_allow_json5 | yajl_allow_comments`
    /// (`yajl.c:77`), and `parseinto.cpp:315-345` never clears them.
    ///
    /// By dialect feature, not by scenario, on BOTH input routes — the
    /// lone-`{...}` JSON-top mode and the `field=<json>` pairs mode.
    mod put_json5_input {
        use super::*;
        use crate::pvdata::ScalarType;

        fn proto() -> FieldDesc {
            FieldDesc::Structure {
                struct_id: "epics:nt/NTScalarArray:1.0".to_string(),
                fields: vec![(
                    "value".to_string(),
                    FieldDesc::ScalarArray(ScalarType::Double),
                )],
            }
        }
        fn tok(args: &[&str]) -> Vec<String> {
            args.iter().map(|s| s.to_string()).collect()
        }
        fn elems(v: &PvField) -> Vec<f64> {
            match v {
                PvField::Structure(s) => {
                    match &s.fields.iter().find(|(n, _)| n == "value").unwrap().1 {
                        PvField::ScalarArray(items) => items
                            .iter()
                            .map(|sv| match sv {
                                ScalarValue::Double(d) => *d,
                                other => panic!("expected Double, got {other:?}"),
                            })
                            .collect(),
                        other => panic!("expected ScalarArray, got {other:?}"),
                    }
                }
                other => panic!("expected Structure, got {other:?}"),
            }
        }

        #[test]
        fn json_top_mode_takes_bare_keys_and_comments() {
            for input in [
                r#"{"value":[1.0,2.0]}"#,       // strict JSON still works
                r#"{value:[1.0,2.0]}"#,         // unquoted identifier key
                "{value:[1.0,2.0]} // two",     // line comment
                "{/* lead */ value:[1.0,2.0]}", // block comment
            ] {
                let (v, _) = build_put_from_args(&proto(), &tok(&[input]), None)
                    .unwrap_or_else(|e| panic!("pvput accepts {input:?}, we must too: {e}"));
                assert_eq!(elems(&v), vec![1.0, 2.0], "input {input:?}");
            }
        }

        #[test]
        fn field_pairs_mode_takes_the_same_dialect() {
            for input in [
                "value=[1.0,2.0]",
                "value=[1.0,2.0] // two",
                "value=[/* lead */1.0,2.0]",
            ] {
                let (v, _) = build_put_from_args(&proto(), &tok(&[input]), None)
                    .unwrap_or_else(|e| panic!("pvput accepts {input:?}, we must too: {e}"));
                assert_eq!(elems(&v), vec![1.0, 2.0], "input {input:?}");
            }
        }
    }

    /// Legacy pvAccessCPP positional bare-token PUT classification
    /// (`pvput.cpp:144-178`): the array-vs-scalar decision is made
    /// against the prototype, so these exercise `build_put_value_from_tokens`
    /// directly with both an NTScalarArray-shaped and a scalar prototype.
    mod put_tokens {
        use super::*;
        use crate::pvdata::ScalarType;

        fn nt_scalar_array() -> FieldDesc {
            FieldDesc::Structure {
                struct_id: "epics:nt/NTScalarArray:1.0".to_string(),
                fields: vec![(
                    "value".to_string(),
                    FieldDesc::ScalarArray(ScalarType::Double),
                )],
            }
        }
        fn nt_scalar() -> FieldDesc {
            FieldDesc::Structure {
                struct_id: "epics:nt/NTScalar:1.0".to_string(),
                fields: vec![("value".to_string(), FieldDesc::Scalar(ScalarType::Double))],
            }
        }
        fn value_array(v: &PvField) -> Vec<f64> {
            match v {
                PvField::Structure(s) => {
                    match &s.fields.iter().find(|(n, _)| n == "value").unwrap().1 {
                        PvField::ScalarArray(items) => items
                            .iter()
                            .map(|sv| match sv {
                                ScalarValue::Double(d) => *d,
                                other => panic!("expected Double element, got {other:?}"),
                            })
                            .collect(),
                        other => panic!("expected ScalarArray .value, got {other:?}"),
                    }
                }
                other => panic!("expected Structure, got {other:?}"),
            }
        }
        fn t(args: &[&str]) -> Vec<String> {
            args.iter().map(|s| s.to_string()).collect()
        }

        /// `pvput arr:pv 3 1.0 2.0` — the leading `3` is the legacy
        /// length and is dropped; `[1.0, 2.0]` is written.
        #[test]
        fn scalar_array_drops_leading_length_token() {
            let v =
                build_put_value_from_tokens(&nt_scalar_array(), &t(&["3", "1.0", "2.0"])).unwrap();
            assert_eq!(value_array(&v), vec![1.0, 2.0]);
        }

        /// A lone `[...]` token is the JSON-array shortcut `value=[...]`
        /// — the brackets are NOT treated as a length-then-element list.
        #[test]
        fn scalar_array_json_bracket_shortcut() {
            let v =
                build_put_value_from_tokens(&nt_scalar_array(), &t(&["[1.0,2.0,3.0]"])).unwrap();
            assert_eq!(value_array(&v), vec![1.0, 2.0, 3.0]);
        }

        /// Degenerate single non-bracket token: the only token is the
        /// length, so the element list is empty (matches upstream
        /// `bare.slice(1)` on a one-element vector).
        #[test]
        fn scalar_array_single_length_token_is_empty() {
            let v = build_put_value_from_tokens(&nt_scalar_array(), &t(&["5"])).unwrap();
            assert!(value_array(&v).is_empty());
        }

        /// A scalar `.value` takes exactly one token; more than one is
        /// rejected ("Can't assign multiple values to scalar").
        #[test]
        fn scalar_rejects_multiple_tokens() {
            assert!(build_put_value_from_tokens(&nt_scalar(), &t(&["1.0"])).is_ok());
            let err = build_put_value_from_tokens(&nt_scalar(), &t(&["1.0", "2.0"])).unwrap_err();
            assert!(
                matches!(err, PvaError::InvalidValue(ref m) if m.contains("multiple values to scalar")),
                "got: {err:?}"
            );
        }
    }

    /// Deferred CLI PUT-token classification (`build_put_from_args`):
    /// the field-vs-bare decision is made against the server prototype,
    /// NOT guessed at parse time. pvAccessCPP `pvtoolsSrc/pvput.cpp:109-235`.
    mod put_from_args {
        use super::*;
        use crate::pvdata::ScalarType;

        /// NTScalar with a typed `.value` plus an `alarm.severity` field,
        /// so `alarm.severity=2` resolves to a real field while an
        /// unknown name does not.
        fn nt(value_type: ScalarType) -> FieldDesc {
            FieldDesc::Structure {
                struct_id: "epics:nt/NTScalar:1.0".to_string(),
                fields: vec![
                    ("value".to_string(), FieldDesc::Scalar(value_type)),
                    (
                        "alarm".to_string(),
                        FieldDesc::Structure {
                            struct_id: "alarm_t".to_string(),
                            fields: vec![
                                ("severity".to_string(), FieldDesc::Scalar(ScalarType::Int)),
                                ("status".to_string(), FieldDesc::Scalar(ScalarType::Int)),
                            ],
                        },
                    ),
                ],
            }
        }
        fn t(args: &[&str]) -> Vec<String> {
            args.iter().map(|s| s.to_string()).collect()
        }
        fn scalar_at<'a>(v: &'a PvField, path: &[&str]) -> &'a ScalarValue {
            let mut cur = v;
            for seg in path {
                match cur {
                    PvField::Structure(s) => {
                        cur = &s.fields.iter().find(|(n, _)| n == seg).unwrap().1;
                    }
                    other => panic!("expected structure at {seg}, got {other:?}"),
                }
            }
            match cur {
                PvField::Scalar(sv) => sv,
                other => panic!("expected scalar at {path:?}, got {other:?}"),
            }
        }

        /// A bare token (no `=`) writes `.value` and marks only the
        /// value bit (pvput.cpp:155-169).
        #[test]
        fn bare_token_targets_value() {
            let intro = nt(ScalarType::Double);
            let (value, changed) = build_put_from_args(&intro, &t(&["42"]), None).unwrap();
            assert_eq!(scalar_at(&value, &["value"]), &ScalarValue::Double(42.0));
            assert!(changed.get(intro.bit_for_path("value").unwrap()));
            assert!(!changed.get(intro.bit_for_path("alarm.severity").unwrap()));
        }

        /// THE PVA-08 case: `a=b` where `a` is not a field and `.value`
        /// is a string scalar is a bare string value, not a field
        /// assignment — `pvput STR:PV a=b` writes the literal `"a=b"`
        /// (pvput.cpp:123-130). The pre-fix CLI rejected this outright.
        #[test]
        fn equals_token_on_string_value_is_bare_string() {
            let intro = nt(ScalarType::String);
            let (value, changed) = build_put_from_args(&intro, &t(&["a=b"]), None).unwrap();
            assert_eq!(
                scalar_at(&value, &["value"]),
                &ScalarValue::String("a=b".into())
            );
            assert!(changed.get(intro.bit_for_path("value").unwrap()));
        }

        /// `field=value` whose field resolves in the prototype is a real
        /// assignment, marking only that field's bit (pvput.cpp:116-121).
        #[test]
        fn existing_field_is_assignment() {
            let intro = nt(ScalarType::Double);
            let (value, changed) =
                build_put_from_args(&intro, &t(&["alarm.severity=2"]), None).unwrap();
            assert_eq!(
                scalar_at(&value, &["alarm", "severity"]),
                &ScalarValue::Int(2)
            );
            assert!(changed.get(intro.bit_for_path("alarm.severity").unwrap()));
            assert!(!changed.get(intro.bit_for_path("value").unwrap()));
        }

        /// An unknown field on a NON-string `.value` is warned-and-ignored,
        /// not a hard error mid-classification; with nothing left it is
        /// "No valid value(s)" (pvput.cpp:131-134,141-143).
        #[test]
        fn unknown_field_on_nonstring_value_is_ignored() {
            let intro = nt(ScalarType::Double);
            let err = build_put_from_args(&intro, &t(&["nope=1"]), None).unwrap_err();
            assert!(
                matches!(err, PvaError::InvalidValue(ref m) if m.contains("No valid value")),
                "got: {err:?}"
            );
        }

        /// Genuine bare values and genuine field pairs cannot mix
        /// (pvput.cpp:139-140) — but this is decided AFTER prototype-aware
        /// classification, not by the mere presence of `=`.
        #[test]
        fn mix_bare_and_field_pair_is_rejected() {
            let intro = nt(ScalarType::Double);
            let err =
                build_put_from_args(&intro, &t(&["42", "alarm.severity=2"]), None).unwrap_err();
            assert!(
                matches!(err, PvaError::InvalidValue(ref m) if m.contains("Can't mix")),
                "got: {err:?}"
            );
        }

        /// Multiple field pairs each mark their own bit in one delta.
        #[test]
        fn multiple_field_pairs_each_marked() {
            let intro = nt(ScalarType::Double);
            let (value, changed) =
                build_put_from_args(&intro, &t(&["alarm.severity=2", "alarm.status=1"]), None)
                    .unwrap();
            assert_eq!(
                scalar_at(&value, &["alarm", "severity"]),
                &ScalarValue::Int(2)
            );
            assert_eq!(
                scalar_at(&value, &["alarm", "status"]),
                &ScalarValue::Int(1)
            );
            assert!(changed.get(intro.bit_for_path("alarm.severity").unwrap()));
            assert!(changed.get(intro.bit_for_path("alarm.status").unwrap()));
            assert!(!changed.get(intro.bit_for_path("value").unwrap()));
        }
    }

    /// JSON value parsing for the CLI PUT classifier (`build_put_from_args`):
    /// a lone `{...}` is JSON top mode, a lone `[...]` is the `value=[...]`
    /// shorthand, and a `field={...}`/`field=[...]` pair lowers JSON into
    /// that field — each marking only the assigned leaves
    /// (pvAccessCPP `pvput.cpp:144-244`, pvData `parseJSON`/`jarray`).
    mod put_json_values {
        use super::*;
        use crate::pvdata::ScalarType;

        /// `{ value: <T>, alarm: { severity: int, status: int } }`.
        fn nt(value_type: ScalarType) -> FieldDesc {
            FieldDesc::Structure {
                struct_id: "epics:nt/NTScalar:1.0".to_string(),
                fields: vec![
                    ("value".to_string(), FieldDesc::Scalar(value_type)),
                    (
                        "alarm".to_string(),
                        FieldDesc::Structure {
                            struct_id: "alarm_t".to_string(),
                            fields: vec![
                                ("severity".to_string(), FieldDesc::Scalar(ScalarType::Int)),
                                ("status".to_string(), FieldDesc::Scalar(ScalarType::Int)),
                            ],
                        },
                    ),
                ],
            }
        }
        /// `{ value: <T>[], alarm: { severity: int, status: int } }`.
        fn nt_array(elem: ScalarType) -> FieldDesc {
            FieldDesc::Structure {
                struct_id: "epics:nt/NTScalarArray:1.0".to_string(),
                fields: vec![
                    ("value".to_string(), FieldDesc::ScalarArray(elem)),
                    (
                        "alarm".to_string(),
                        FieldDesc::Structure {
                            struct_id: "alarm_t".to_string(),
                            fields: vec![
                                ("severity".to_string(), FieldDesc::Scalar(ScalarType::Int)),
                                ("status".to_string(), FieldDesc::Scalar(ScalarType::Int)),
                            ],
                        },
                    ),
                ],
            }
        }
        fn t(args: &[&str]) -> Vec<String> {
            args.iter().map(|s| s.to_string()).collect()
        }
        fn scalar_at<'a>(v: &'a PvField, path: &[&str]) -> &'a ScalarValue {
            let mut cur = v;
            for seg in path {
                match cur {
                    PvField::Structure(s) => {
                        cur = &s.fields.iter().find(|(n, _)| n == seg).unwrap().1;
                    }
                    other => panic!("expected structure at {seg}, got {other:?}"),
                }
            }
            match cur {
                PvField::Scalar(sv) => sv,
                other => panic!("expected scalar at {path:?}, got {other:?}"),
            }
        }
        fn array_at<'a>(v: &'a PvField, name: &str) -> &'a [ScalarValue] {
            match v {
                PvField::Structure(s) => {
                    match &s.fields.iter().find(|(n, _)| n == name).unwrap().1 {
                        PvField::ScalarArray(items) => items,
                        other => panic!("expected ScalarArray at {name}, got {other:?}"),
                    }
                }
                other => panic!("expected Structure, got {other:?}"),
            }
        }

        /// JSON top mode (`pvput.cpp:150-153`): a lone `{...}` parses into the
        /// root, assigning each named leaf and marking ONLY those leaves — an
        /// unmentioned sibling (`alarm.status`) stays unmarked so the server
        /// keeps its current value.
        #[test]
        fn json_top_mode_marks_only_named_leaves() {
            let intro = nt(ScalarType::Double);
            let (value, changed) = build_put_from_args(
                &intro,
                &t(&[r#"{"value":42,"alarm":{"severity":2}}"#]),
                None,
            )
            .unwrap();
            assert_eq!(scalar_at(&value, &["value"]), &ScalarValue::Double(42.0));
            assert_eq!(
                scalar_at(&value, &["alarm", "severity"]),
                &ScalarValue::Int(2)
            );
            assert!(changed.get(intro.bit_for_path("value").unwrap()));
            assert!(changed.get(intro.bit_for_path("alarm.severity").unwrap()));
            // The unmentioned sibling is NOT marked.
            assert!(!changed.get(intro.bit_for_path("alarm.status").unwrap()));
            // The container bit is not blanket-marked either.
            assert!(!changed.get(intro.bit_for_path("alarm").unwrap()));
        }

        /// Field-mode JSON object (`pvput.cpp:231-233`): `alarm={...}` lowers
        /// the object into the `alarm` sub-structure, marking each assigned
        /// leaf — `value` and `alarm.status` stay unmarked.
        #[test]
        fn json_field_object_marks_assigned_leaves() {
            let intro = nt(ScalarType::Double);
            let (value, changed) =
                build_put_from_args(&intro, &t(&[r#"alarm={"severity":3}"#]), None).unwrap();
            assert_eq!(
                scalar_at(&value, &["alarm", "severity"]),
                &ScalarValue::Int(3)
            );
            assert!(changed.get(intro.bit_for_path("alarm.severity").unwrap()));
            assert!(!changed.get(intro.bit_for_path("alarm.status").unwrap()));
            assert!(!changed.get(intro.bit_for_path("value").unwrap()));
        }

        /// Field-mode JSON array (`pvput.cpp:219-229`, jarray → scalarArray):
        /// `value=[...]` writes the typed array and marks the value bit.
        #[test]
        fn json_field_array_writes_scalar_array() {
            let intro = nt_array(ScalarType::Double);
            let (value, changed) =
                build_put_from_args(&intro, &t(&["value=[1.0,2.0,3.0]"]), None).unwrap();
            let got: Vec<f64> = array_at(&value, "value")
                .iter()
                .map(|sv| match sv {
                    ScalarValue::Double(d) => *d,
                    other => panic!("expected Double, got {other:?}"),
                })
                .collect();
            assert_eq!(got, vec![1.0, 2.0, 3.0]);
            assert!(changed.get(intro.bit_for_path("value").unwrap()));
        }

        /// A lone `[...]` is the `value=[...]` shorthand (`pvput.cpp:144-148`)
        /// — routed through the JSON-array field path, not the legacy
        /// length-then-element list.
        #[test]
        fn lone_bracket_is_value_shorthand() {
            let intro = nt_array(ScalarType::Int);
            let (value, changed) = build_put_from_args(&intro, &t(&["[10,20,30]"]), None).unwrap();
            let got: Vec<i32> = array_at(&value, "value")
                .iter()
                .map(|sv| match sv {
                    ScalarValue::Int(i) => *i,
                    other => panic!("expected Int, got {other:?}"),
                })
                .collect();
            assert_eq!(got, vec![10, 20, 30]);
            assert!(changed.get(intro.bit_for_path("value").unwrap()));
        }

        /// An unknown JSON key is a hard error, like pvData `getSubFieldT`
        /// (`parseinto.cpp:212`) — silently dropping it would lose a value
        /// the user asked to write.
        #[test]
        fn json_unknown_key_is_error() {
            let intro = nt(ScalarType::Double);
            let err = build_put_from_args(&intro, &t(&[r#"{"nope":1}"#]), None).unwrap_err();
            assert!(
                matches!(err, PvaError::InvalidValue(ref m) if m.contains("not present")),
                "got: {err:?}"
            );
        }

        /// JSON string coercion into a numeric leaf matches pvData
        /// `putFrom` (`parseinto.cpp:60-66`): `"7"` lands in an int field.
        #[test]
        fn json_string_coerces_into_numeric_field() {
            let intro = nt(ScalarType::Double);
            let (value, changed) =
                build_put_from_args(&intro, &t(&[r#"alarm={"severity":"7"}"#]), None).unwrap();
            assert_eq!(
                scalar_at(&value, &["alarm", "severity"]),
                &ScalarValue::Int(7)
            );
            assert!(changed.get(intro.bit_for_path("alarm.severity").unwrap()));
        }
    }

    /// NTEnum bare-token PUT (`build_put_from_args` plain value mode):
    /// a bare token writes `value.index` — matched against the current
    /// `value.choices` by label first, then parsed as an integer index —
    /// and marks ONLY `value.index`, so the server keeps its choice menu
    /// (pvAccessCPP `pvput.cpp:180-204`).
    mod put_enum_value {
        use super::*;
        use crate::pvdata::ScalarType;

        /// `{ value: enum_t{ index: int, choices: string[] }, alarm: {...} }`.
        fn nt_enum() -> FieldDesc {
            FieldDesc::Structure {
                struct_id: "epics:nt/NTEnum:1.0".to_string(),
                fields: vec![
                    (
                        "value".to_string(),
                        FieldDesc::Structure {
                            struct_id: "enum_t".to_string(),
                            fields: vec![
                                ("index".to_string(), FieldDesc::Scalar(ScalarType::Int)),
                                (
                                    "choices".to_string(),
                                    FieldDesc::ScalarArray(ScalarType::String),
                                ),
                            ],
                        },
                    ),
                    (
                        "alarm".to_string(),
                        FieldDesc::Structure {
                            struct_id: "alarm_t".to_string(),
                            fields: vec![(
                                "severity".to_string(),
                                FieldDesc::Scalar(ScalarType::Int),
                            )],
                        },
                    ),
                ],
            }
        }
        /// A get-first snapshot whose `value.choices` are the given labels.
        fn previous_with_choices(choices: &[&str]) -> PvField {
            let mut enum_v = PvStructure::new("enum_t");
            enum_v
                .fields
                .push(("index".to_string(), PvField::Scalar(ScalarValue::Int(0))));
            enum_v.fields.push((
                "choices".to_string(),
                PvField::ScalarArray(
                    choices
                        .iter()
                        .map(|c| ScalarValue::String((*c).into()))
                        .collect(),
                ),
            ));
            let mut root = PvStructure::new("epics:nt/NTEnum:1.0");
            root.fields
                .push(("value".to_string(), PvField::Structure(enum_v)));
            PvField::Structure(root)
        }
        fn t(args: &[&str]) -> Vec<String> {
            args.iter().map(|s| s.to_string()).collect()
        }
        fn index_of(v: &PvField) -> i32 {
            match v {
                PvField::Structure(s) => {
                    let value = &s.fields.iter().find(|(n, _)| n == "value").unwrap().1;
                    match value {
                        PvField::Structure(e) => {
                            match &e.fields.iter().find(|(n, _)| n == "index").unwrap().1 {
                                PvField::Scalar(ScalarValue::Int(i)) => *i,
                                other => panic!("expected Int index, got {other:?}"),
                            }
                        }
                        other => panic!("expected enum_t value, got {other:?}"),
                    }
                }
                other => panic!("expected Structure, got {other:?}"),
            }
        }

        /// A token equal to a choice label writes that choice's position
        /// (pvput.cpp:190-197) and marks ONLY `value.index` — `value` and
        /// `value.choices` stay unmarked so the menu is preserved.
        #[test]
        fn label_resolves_to_index_marking_only_value_index() {
            let intro = nt_enum();
            let previous = previous_with_choices(&["OFF", "ON", "AUTO"]);
            let (value, changed) =
                build_put_from_args(&intro, &t(&["AUTO"]), Some(&previous)).unwrap();
            assert_eq!(index_of(&value), 2);
            assert!(changed.get(intro.bit_for_path("value.index").unwrap()));
            // The container and the choices array must NOT be marked — else
            // the server would overwrite its menu with an empty array.
            assert!(!changed.get(intro.bit_for_path("value").unwrap()));
            assert!(!changed.get(intro.bit_for_path("value.choices").unwrap()));
        }

        /// The wire decode yields `value.choices` as the canonical typed
        /// array ([`PvField::ScalarArrayTyped`]), not the untyped
        /// [`PvField::ScalarArray`] the other fixtures hand-build. Label
        /// resolution must accept it too — otherwise an enum-by-label put
        /// over the wire (where the get-first snapshot always decodes typed)
        /// silently falls through to the integer fallback and rejects every
        /// real label.
        #[test]
        fn label_resolves_via_typed_choices_array() {
            let intro = nt_enum();
            let mut enum_v = PvStructure::new("enum_t");
            enum_v
                .fields
                .push(("index".to_string(), PvField::Scalar(ScalarValue::Int(0))));
            enum_v.fields.push((
                "choices".to_string(),
                PvField::ScalarArrayTyped(crate::pvdata::TypedScalarArray::String(
                    ["OFF", "ON", "AUTO"]
                        .into_iter()
                        .map(epics_base_rs::types::PvString::from)
                        .collect(),
                )),
            ));
            let mut root = PvStructure::new("epics:nt/NTEnum:1.0");
            root.fields
                .push(("value".to_string(), PvField::Structure(enum_v)));
            let previous = PvField::Structure(root);

            let (value, changed) =
                build_put_from_args(&intro, &t(&["AUTO"]), Some(&previous)).unwrap();
            assert_eq!(
                index_of(&value),
                2,
                "typed-array choices must resolve the label to its position"
            );
            assert!(changed.get(intro.bit_for_path("value.index").unwrap()));
            assert!(!changed.get(intro.bit_for_path("value.choices").unwrap()));
        }

        /// A token that is not a choice label is parsed as the integer index
        /// (pvput.cpp:199-202), still marking only `value.index`.
        #[test]
        fn integer_token_falls_back_to_index() {
            let intro = nt_enum();
            let previous = previous_with_choices(&["OFF", "ON", "AUTO"]);
            let (value, changed) =
                build_put_from_args(&intro, &t(&["1"]), Some(&previous)).unwrap();
            assert_eq!(index_of(&value), 1);
            assert!(changed.get(intro.bit_for_path("value.index").unwrap()));
            assert!(!changed.get(intro.bit_for_path("value.choices").unwrap()));
        }

        /// A choice label that happens to be numeric still wins over the
        /// integer fallback (pvput matches the label first): choice `"1"`
        /// at position 0 resolves to index 0, not index 1.
        #[test]
        fn numeric_label_wins_over_integer_parse() {
            let intro = nt_enum();
            let previous = previous_with_choices(&["1", "2"]);
            let (value, _changed) =
                build_put_from_args(&intro, &t(&["1"]), Some(&previous)).unwrap();
            assert_eq!(index_of(&value), 0);
        }

        /// Without a get-first snapshot a label cannot be resolved, so a
        /// non-integer token is a clear error rather than a silent index 0.
        #[test]
        fn label_without_snapshot_is_error() {
            let intro = nt_enum();
            let err = build_put_from_args(&intro, &t(&["AUTO"]), None).unwrap_err();
            assert!(
                matches!(err, PvaError::InvalidValue(ref m) if m.contains("neither a choice label")),
                "got: {err:?}"
            );
        }

        /// An integer token still works with no snapshot (the fallback path
        /// needs no choices).
        #[test]
        fn integer_token_without_snapshot_ok() {
            let intro = nt_enum();
            let (value, changed) = build_put_from_args(&intro, &t(&["2"]), None).unwrap();
            assert_eq!(index_of(&value), 2);
            assert!(changed.get(intro.bit_for_path("value.index").unwrap()));
        }

        /// More than one bare token is rejected for an enum
        /// (pvput.cpp:181-183).
        #[test]
        fn multiple_tokens_rejected() {
            let intro = nt_enum();
            let previous = previous_with_choices(&["OFF", "ON"]);
            let err = build_put_from_args(&intro, &t(&["ON", "OFF"]), Some(&previous)).unwrap_err();
            assert!(
                matches!(err, PvaError::InvalidValue(ref m) if m.contains("multiple values to enum")),
                "got: {err:?}"
            );
        }
    }

    /// Typed multi-field PUT delta (`build_field_delta` with `PutLeaf`):
    /// a typed pvData leaf is assigned as-is, a text leaf is parsed —
    /// the combined sibling-field PUT path used by pvalink OUT links.
    mod field_delta_typed {
        use super::*;
        use crate::pvdata::ScalarType;

        /// `{ value: Double[], gain: Int }` — an array field plus a
        /// scalar sibling, so a combined PUT can carry one typed array
        /// and one parsed scalar.
        fn proto() -> FieldDesc {
            FieldDesc::Structure {
                struct_id: "test:array_plus_scalar".to_string(),
                fields: vec![
                    (
                        "value".to_string(),
                        FieldDesc::ScalarArray(ScalarType::Double),
                    ),
                    ("gain".to_string(), FieldDesc::Scalar(ScalarType::Int)),
                ],
            }
        }

        fn array_at<'a>(v: &'a PvField, name: &str) -> &'a [ScalarValue] {
            match v {
                PvField::Structure(s) => {
                    match &s.fields.iter().find(|(n, _)| n == name).unwrap().1 {
                        PvField::ScalarArray(items) => items,
                        other => panic!("expected ScalarArray at {name}, got {other:?}"),
                    }
                }
                other => panic!("expected Structure, got {other:?}"),
            }
        }
        fn scalar_at<'a>(v: &'a PvField, name: &str) -> &'a ScalarValue {
            match v {
                PvField::Structure(s) => {
                    match &s.fields.iter().find(|(n, _)| n == name).unwrap().1 {
                        PvField::Scalar(sv) => sv,
                        other => panic!("expected Scalar at {name}, got {other:?}"),
                    }
                }
                other => panic!("expected Structure, got {other:?}"),
            }
        }

        /// A typed array leaf is assigned verbatim — its element type and
        /// values survive intact — while a sibling text leaf is parsed.
        /// This is exactly what stringifying the array (the pre-fix
        /// `queued_to_string` path) would destroy: `Display` renders the
        /// array as `[1, 2, 3]`, which the field parser then splits on
        /// commas into `"[1"`, `"2"`, `"3]"` and fails/corrupts.
        #[test]
        fn typed_array_leaf_assigned_verbatim_with_str_sibling() {
            let intro = proto();
            let arr = PvField::ScalarArray(vec![
                ScalarValue::Double(1.0),
                ScalarValue::Double(2.0),
                ScalarValue::Double(3.0),
            ]);
            let assignments = vec![
                ("value".to_string(), PutLeaf::Typed(arr)),
                ("gain".to_string(), PutLeaf::Str("7".to_string())),
            ];
            let (value, changed) = build_field_delta(&intro, &assignments).unwrap();

            assert_eq!(
                array_at(&value, "value"),
                &[
                    ScalarValue::Double(1.0),
                    ScalarValue::Double(2.0),
                    ScalarValue::Double(3.0),
                ],
                "typed array leaf must reach the delta unchanged, not stringified",
            );
            assert_eq!(scalar_at(&value, "gain"), &ScalarValue::Int(7));
            assert!(changed.get(intro.bit_for_path("value").unwrap()));
            assert!(changed.get(intro.bit_for_path("gain").unwrap()));

            // The typed delta must encode/decode against the prototype —
            // proves the assigned array is wire-valid, not merely typed.
            for order in [ByteOrder::Little, ByteOrder::Big] {
                let mut buf = Vec::new();
                encode_pv_field_with_bitset(&value, &intro, &changed, 0, order, &mut buf);
                assert!(!buf.is_empty(), "typed delta encoded empty ({order:?})");
            }
        }

        /// A typed leaf for a path absent from the prototype is rejected,
        /// same as the text path (no silent drop).
        #[test]
        fn typed_leaf_unknown_path_rejected() {
            let intro = proto();
            let assignments = vec![(
                "nope".to_string(),
                PutLeaf::Typed(PvField::Scalar(ScalarValue::Int(1))),
            )];
            let err = build_field_delta(&intro, &assignments).unwrap_err();
            assert!(
                matches!(err, PvaError::InvalidValue(ref m) if m.contains("not present in introspection")),
                "got: {err:?}"
            );
        }
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
    // that policy; both malformed cases must be `Invalid` (circuit-fatal).

    #[test]
    fn bfr11_too_short_frame_is_invalid_decode() {
        // < 5 bytes: no room for ioid (4) + subcmd (1).
        match classify_raw_monitor_frame(&[0, 0, 0], ByteOrder::Little) {
            RawMonitorFrameKind::Invalid(PvaError::Decode(msg)) => {
                assert!(msg.contains("too short"), "msg: {msg}");
            }
            other => panic!("too-short frame must be Invalid(Decode), got {other:?}"),
        }
    }

    #[test]
    fn bfr11_finish_truncated_status_is_invalid_decode() {
        // subcmd 0x10 (FINISH) but NO status bytes after it → the
        // required Status cannot decode.
        let payload = [0u8, 0, 0, 0, 0x10];
        match classify_raw_monitor_frame(&payload, ByteOrder::Little) {
            RawMonitorFrameKind::Invalid(PvaError::Decode(msg)) => {
                assert!(msg.contains("FINISH status"), "msg: {msg}");
            }
            other => panic!("truncated FINISH status must be Invalid(Decode), got {other:?}"),
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

    /// R6-35: a FINISH frame whose Status is followed by more bytes carries one
    /// last monitor update. pvxs decodes it (`clientmon.cpp:504-511`:
    /// `else if(!final || !M.empty())`) and queues it before the `Finished()`
    /// marker, so the raw-forwarding loop must relay that body downstream
    /// instead of dropping the frame as a bare end-of-stream.
    #[test]
    fn finish_with_a_trailing_update_carries_a_body() {
        let mut payload = vec![0u8, 0, 0, 0, 0x10];
        crate::proto::Status::ok().write_into(ByteOrder::Little, &mut payload);
        let status_end = payload.len();
        // changed | value | overrun — opaque to this loop, relayed verbatim.
        payload.extend_from_slice(&[0x01, 0x02, 0x2a, 0x00]);
        match classify_raw_monitor_frame(&payload, ByteOrder::Little) {
            RawMonitorFrameKind::FinishData { body_start } => {
                assert_eq!(body_start, status_end);
                assert_eq!(&payload[body_start..], &[0x01, 0x02, 0x2a, 0x00]);
            }
            other => panic!("FINISH with a trailing update must be FinishData, got {other:?}"),
        }
    }

    /// The boundary either side of it: a FINISH with NOTHING after the Status is
    /// still the plain end-of-stream, not a zero-length body.
    #[test]
    fn finish_with_no_trailing_bytes_stays_a_clean_end() {
        let mut payload = vec![0u8, 0, 0, 0, 0x10];
        crate::proto::Status::ok().write_into(ByteOrder::Little, &mut payload);
        assert!(matches!(
            classify_raw_monitor_frame(&payload, ByteOrder::Little),
            RawMonitorFrameKind::FinishOk
        ));
    }

    /// A FINISH whose Status DECODES but reports an error is a remote error on
    /// a well-formed frame, not a wire fault: pvxs hands the subscription a
    /// `RemoteError` and leaves the circuit up (clientmon.cpp:612-614). It must
    /// classify as `FinishError`, NOT `Invalid` — an `Invalid` here would take
    /// the whole circuit (and every other channel on it) down over one
    /// subscription's error status.
    #[test]
    fn bfr11_finish_error_status_is_op_local_finish_error() {
        let mut payload = vec![0u8, 0, 0, 0, 0x10];
        crate::proto::Status::error("boom".to_string()).write_into(ByteOrder::Little, &mut payload);
        match classify_raw_monitor_frame(&payload, ByteOrder::Little) {
            // The peer's own Status, not a rendering of it — a forwarding
            // gateway sends it downstream unchanged (R18-27).
            RawMonitorFrameKind::FinishError(PvaError::RemoteError(st)) => {
                assert_eq!(st, crate::proto::Status::error("boom".to_string()));
            }
            other => panic!("non-success FINISH must be FinishError, got {other:?}"),
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

    /// A second INIT
    /// (`subcmd & 0x08`) on a running raw monitor is a state-machine
    /// violation: pvxs faults the buffer and resets the connection
    /// (clientmon.cpp:589-605). The classifier must surface it as
    /// `Invalid`, never swallow it as a benign skipped control frame.
    #[test]
    fn raw_monitor_second_init_is_invalid_protocol() {
        // subcmd 0x08 (INIT) with a trailing body (Status + descriptor
        // shape) — irrelevant, since the frame is rejected on the subcmd.
        let payload = [0u8, 0, 0, 0, 0x08, 0xff, 0x00];
        match classify_raw_monitor_frame(&payload, ByteOrder::Little) {
            RawMonitorFrameKind::Invalid(PvaError::Protocol(msg)) => {
                assert!(msg.contains("INIT"), "msg: {msg}");
            }
            other => panic!("second INIT must be Invalid(Protocol), got {other:?}"),
        }
    }

    /// Structural. A
    /// server emits only DATA (0x00), INIT (0x08), and FINISH (0x10) on a
    /// monitor stream (servermon.cpp:133-149); START/STOP/ACK are
    /// client->server only. The classifier no longer treats an unexpected
    /// subcmd as a benign skip — it is a protocol violation, matching
    /// pvxs's decode-fault + reset on an out-of-state monitor frame.
    #[test]
    fn raw_monitor_unexpected_subcmd_is_invalid_protocol() {
        // subcmd 0x04 (STOP) is a client->server control byte; a server
        // never sends it on the monitor stream.
        let payload = [0u8, 0, 0, 0, 0x04];
        match classify_raw_monitor_frame(&payload, ByteOrder::Little) {
            RawMonitorFrameKind::Invalid(PvaError::Protocol(msg)) => {
                assert!(msg.contains("unexpected subcmd"), "msg: {msg}");
            }
            other => panic!("unexpected subcmd must be Invalid(Protocol), got {other:?}"),
        }
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

    fn opts(pairs: &[(&str, &str)]) -> Vec<(String, crate::pvdata::ScalarValue)> {
        // The parsed-text option form: every value is string-typed.
        // `from_record_options` normalises typed and string options
        // through `to_string()`, so these still exercise the same paths.
        pairs
            .iter()
            .map(|(k, v)| {
                (
                    k.to_string(),
                    crate::pvdata::ScalarValue::String(v.to_string().into()),
                )
            })
            .collect()
    }

    /// A custom monitor pvRequest's pipeline
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

        // pipeline=false → no trailer and no ACK, but the queue depth is
        // still resolved: pvxs's `queueSize` block sits outside any pipeline
        // gate (clientmon.cpp:761-773) and its value is what `stats()` reports
        // as `limitQueue` (:156). R10-35 — this used to answer 0.
        let f = MonitorFlow::from_record_options(&opts(&[("pipeline", "false")]), 4);
        assert!(!f.pipeline);
        assert_eq!(f.queue_size, 4, "the builder default depth still stands");

        // No pipeline option at all → pvxs default false, and an explicit
        // queueSize is STILL honored (it is the client's queue depth, and it
        // travels in the pvRequest as the server's squash depth).
        let f = MonitorFlow::from_record_options(&opts(&[("queueSize", "16")]), 4);
        assert!(!f.pipeline);
        assert_eq!(f.queue_size, 16);

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

    /// Build a decoded pvRequest VALUE carrying `record._options.<pairs>`,
    /// the shape the gateway forwards into the raw-frame monitor path.
    fn pv_request_with_options(pairs: &[(&str, PvField)]) -> PvField {
        use crate::pvdata::PvStructure;
        let options_value = PvField::Structure(PvStructure {
            struct_id: String::new(),
            fields: pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.clone()))
                .collect(),
        });
        let record_value = PvField::Structure(PvStructure {
            struct_id: String::new(),
            fields: vec![("_options".to_string(), options_value)],
        });
        PvField::Structure(PvStructure {
            struct_id: String::new(),
            fields: vec![("record".to_string(), record_value)],
        })
    }

    /// The raw-frame monitor path must derive its `MonitorFlow` from the
    /// forwarded request's own `record._options` — including TYPED values a
    /// pvxs builder client sends (`Boolean`/`UInt`), not just the parsed
    /// string form — so the wire pipeline/queueSize and the ACK cadence
    /// share one origin. `ackAny=75%` of 16 → ackAt 12 (pvxs parity).
    #[test]
    fn record_options_from_request_extracts_typed_pipeline_options() {
        let req = pv_request_with_options(&[
            ("pipeline", PvField::Scalar(ScalarValue::Boolean(true))),
            ("queueSize", PvField::Scalar(ScalarValue::UInt(16))),
            ("ackAny", PvField::Scalar(ScalarValue::String("75%".into()))),
        ]);
        let extracted = record_options_from_request(&req);
        assert_eq!(
            extracted.len(),
            3,
            "all three scalar options must be extracted"
        );
        let flow = MonitorFlow::from_record_options(&extracted, 4);
        assert!(flow.pipeline);
        assert_eq!(flow.queue_size, 16);
        assert_eq!(flow.ack_at, 12);
    }

    /// A forwarded plain monitor request (no `record._options`) must
    /// extract nothing and derive a plain (non-pipeline) flow — pvxs
    /// servers enable pipeline only from the pvRequest, so the client must
    /// not send a `nack` trailer / ACKs the server would ignore. The queue
    /// depth is still the caller's default (pvxs `SubscriptionImpl` seeds
    /// `queueSize=4` and only `pipeline` gates the trailer).
    #[test]
    fn record_options_from_request_plain_request_yields_no_pipeline() {
        let req = PvField::Structure(crate::pvdata::PvStructure {
            struct_id: String::new(),
            fields: vec![],
        });
        assert!(record_options_from_request(&req).is_empty());
        let flow = MonitorFlow::from_record_options(&record_options_from_request(&req), 4);
        assert!(!flow.pipeline);
        assert_eq!(flow.queue_size, 4);
    }

    /// A forwarded request naming `queueSize` but NOT `pipeline` must stay
    /// plain (pvxs `clientmon.cpp` defaults `pipeline` false): the gateway
    /// forwarding such a request opens a plain upstream monitor, not a
    /// pipelined one driven by the client's builder default. The depth it
    /// asks for is still honored — `queueSize` is parsed outside the
    /// `pipeline` gate in pvxs (clientmon.cpp:761-773).
    #[test]
    fn record_options_queue_size_without_pipeline_stays_plain() {
        let req = pv_request_with_options(&[("queueSize", PvField::Scalar(ScalarValue::UInt(16)))]);
        let extracted = record_options_from_request(&req);
        assert_eq!(extracted.len(), 1);
        let flow = MonitorFlow::from_record_options(&extracted, 4);
        assert!(!flow.pipeline);
        assert_eq!(flow.queue_size, 16);
    }

    /// R10-35 — the option values are CONVERTED (pvxs `Value::as<T>()`), not
    /// string-matched against a display-normalized rendering. Each case here
    /// fails on the pre-fix parser, which compared `v.to_string()` to the
    /// literals `"true"`/`"1"` and ran `str::parse::<u32>()`.
    #[test]
    fn record_options_convert_like_pvxs_value_as() {
        // `pipeline` goes through `as(bool)`: any non-zero integer or real is
        // true. The old parser saw "2"/"-1"/"0.5" and answered false.
        for truthy in [
            ScalarValue::Int(2),
            ScalarValue::Int(-1),
            ScalarValue::Double(0.5),
            ScalarValue::UByte(7),
        ] {
            let req = pv_request_with_options(&[("pipeline", PvField::Scalar(truthy.clone()))]);
            let flow = MonitorFlow::from_record_options(&record_options_from_request(&req), 4);
            assert!(flow.pipeline, "{truthy:?} must convert to pipeline=true");
        }
        // ... and a zero of any storage is false (negative control).
        for falsy in [ScalarValue::Int(0), ScalarValue::Double(0.0)] {
            let req = pv_request_with_options(&[("pipeline", PvField::Scalar(falsy.clone()))]);
            let flow = MonitorFlow::from_record_options(&record_options_from_request(&req), 4);
            assert!(!flow.pipeline, "{falsy:?} must convert to pipeline=false");
        }

        // `queueSize` goes through `as(uint32)`: strings are base-0 (so `0x10`
        // is 16, matching `stoull(s, &idx, 0)`), and reals truncate.
        let req = pv_request_with_options(&[(
            "queueSize",
            PvField::Scalar(ScalarValue::String("0x10".into())),
        )]);
        let flow = MonitorFlow::from_record_options(&record_options_from_request(&req), 4);
        assert_eq!(flow.queue_size, 16, "string queueSize is base-0 like pvxs");

        let req =
            pv_request_with_options(&[("queueSize", PvField::Scalar(ScalarValue::Double(8.5)))]);
        let flow = MonitorFlow::from_record_options(&record_options_from_request(&req), 4);
        assert_eq!(flow.queue_size, 8, "real queueSize truncates like a C cast");

        // An unconvertible `queueSize` falls back to the caller's default —
        // pvxs's `catch(std::exception&)` around the conversion (:769-772).
        let req = pv_request_with_options(&[(
            "queueSize",
            PvField::Scalar(ScalarValue::String("garbage".into())),
        )]);
        let flow = MonitorFlow::from_record_options(&record_options_from_request(&req), 4);
        assert_eq!(flow.queue_size, 4);

        // `ackAny` percent syntax applies ONLY to String storage
        // (clientmon.cpp:783 checks `ackAny.type()==TypeCode::String` first);
        // every other storage is an absolute count via `as(uint32)`.
        let req = pv_request_with_options(&[
            ("queueSize", PvField::Scalar(ScalarValue::UInt(16))),
            ("ackAny", PvField::Scalar(ScalarValue::UInt(2))),
        ]);
        let flow = MonitorFlow::from_record_options(&record_options_from_request(&req), 4);
        assert_eq!(flow.ack_at, 2);

        // A typed real ackAny converts too — the old parser only handled a
        // decimal string.
        let req = pv_request_with_options(&[
            ("queueSize", PvField::Scalar(ScalarValue::UInt(16))),
            ("ackAny", PvField::Scalar(ScalarValue::Double(3.0))),
        ]);
        let flow = MonitorFlow::from_record_options(&record_options_from_request(&req), 4);
        assert_eq!(flow.ack_at, 3);

        // Unconvertible `ackAny` → pvxs's `ackAt = queueSize/2` fallback,
        // clamped into [1, queueSize].
        let req = pv_request_with_options(&[
            ("queueSize", PvField::Scalar(ScalarValue::UInt(16))),
            (
                "ackAny",
                PvField::Scalar(ScalarValue::String("junk".into())),
            ),
        ]);
        let flow = MonitorFlow::from_record_options(&record_options_from_request(&req), 4);
        assert_eq!(flow.ack_at, 8);
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

    /// A monitor loop parked in
    /// `stream.recv().await` waits on `cancel` via `select!`. The single
    /// teardown owner must wake it. Model the loop's wait with a task
    /// that only awaits `cancel.notified()`; `teardown()` must make it
    /// return promptly instead of hanging forever.
    #[epics_macros_rs::epics_test]
    async fn teardown_wakes_a_parked_monitor_loop() {
        let state = idle_sub_state();
        let task_state = state.clone();
        let task = crate::test_reactor().spawn(async move {
            task_state.cancel.notified().await;
        });
        // Give the task a chance to reach `.notified().await`.
        epics_base_rs::runtime::task::yield_now().await;
        state.teardown();
        epics_base_rs::runtime::task::timeout(std::time::Duration::from_secs(1), task)
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
    #[epics_macros_rs::epics_test]
    async fn teardown_before_park_is_not_lost() {
        let state = idle_sub_state();
        state.teardown(); // notify_one() with no waiter -> stored permit.
        epics_base_rs::runtime::task::timeout(
            std::time::Duration::from_secs(1),
            state.cancel.notified(),
        )
        .await
        .expect("stored cancel permit must satisfy a later notified()");
    }

    /// `teardown()` is the shared owner for `stop()`, `stop_sync()`, and
    /// `Drop`; calling it more than once (e.g. `stop_sync` then `Drop`)
    /// must be a harmless no-op.
    #[epics_macros_rs::epics_test]
    async fn teardown_is_idempotent() {
        let state = idle_sub_state();
        state.teardown();
        state.teardown();
        assert!(state.stop.load(std::sync::atomic::Ordering::Relaxed));
        assert!(state.active.lock().is_none());
    }

    fn dummy_monitor_frame() -> Frame {
        let payload = vec![0u8; 4];
        let header = PvaHeader::application(
            true,
            ByteOrder::Little,
            Command::Monitor.code(),
            payload.len() as u32,
        );
        Frame { header, payload }
    }

    /// The MONITOR INIT receive is the await between `register_ioid_monitor`
    /// and publishing `active`. A `stop_sync()`/teardown issued while the
    /// server withholds the INIT reply must complete the cancel promptly
    /// (return `Cancelled`) rather than hang the spawned monitor task
    /// forever — the regression this fix closes.
    #[epics_macros_rs::epics_test]
    async fn recv_monitor_init_cancels_on_teardown_during_wait() {
        let state = idle_sub_state();
        let task_state = Some(state.clone());
        let (tx, rx) = super::super::monitor_queue::MonitorSink::new(4, false);
        let task =
            crate::test_reactor().spawn(async move { recv_monitor_init(&task_state, &rx).await });
        // Let the task reach its `select!` (silent-but-open server).
        epics_base_rs::runtime::task::yield_now().await;
        state.teardown();
        let out = epics_base_rs::runtime::task::timeout(std::time::Duration::from_secs(1), task)
            .await
            .expect("INIT recv must wake on teardown, not hang on a silent server")
            .expect("task panicked");
        assert!(
            matches!(out, MonitorInit::Cancelled),
            "a teardown during the INIT wait must yield Cancelled"
        );
        // Keep the sender alive until here so the stream modelled an open
        // (silent) server, not a closed one.
        drop(tx);
    }

    /// A teardown that races just ahead of the INIT receive sets `stop`
    /// before the loop reaches the await; the pre-check must short-circuit
    /// to `Cancelled` WITHOUT consuming a reply that may already be queued.
    #[epics_macros_rs::epics_test]
    async fn recv_monitor_init_cancels_when_stop_preset() {
        let state = idle_sub_state();
        state.stop.store(true, Ordering::Relaxed);
        let opt = Some(state.clone());
        let (tx, rx) = super::super::monitor_queue::MonitorSink::new(4, false);
        tx.push(dummy_monitor_frame()); // a reply is even available
        match recv_monitor_init(&opt, &rx).await {
            MonitorInit::Cancelled => {}
            _ => panic!("a preset stop must short-circuit to Cancelled"),
        }
        assert_eq!(
            rx.counters().2,
            1,
            "preset stop must not consume the queued INIT reply"
        );
    }

    /// The happy path: a queued INIT reply is delivered as `Reply`.
    #[epics_macros_rs::epics_test]
    async fn recv_monitor_init_returns_reply_when_frame_arrives() {
        let state = Some(idle_sub_state());
        let (tx, rx) = super::super::monitor_queue::MonitorSink::new(4, false);
        tx.push(dummy_monitor_frame());
        match recv_monitor_init(&state, &rx).await {
            MonitorInit::Reply(_) => {}
            _ => panic!("a queued INIT reply must yield Reply"),
        }
    }

    /// A frame stream closed before any reply is `Lost` (connection lost),
    /// distinct from a cancel — the caller maps it to ConnectionLost and
    /// lets the reconnect loop retry.
    #[epics_macros_rs::epics_test]
    async fn recv_monitor_init_lost_when_stream_closed() {
        let state = Some(idle_sub_state());
        let (tx, rx) = super::super::monitor_queue::MonitorSink::new(4, false);
        drop(tx); // server connection gone, no reply will arrive
        match recv_monitor_init(&state, &rx).await {
            MonitorInit::Lost => {}
            _ => panic!("a closed stream before the reply must yield Lost"),
        }
    }

    /// The no-handle path (plain `op_monitor`, `state == None`) cannot be
    /// cancelled, so it still simply awaits and delivers the reply.
    #[epics_macros_rs::epics_test]
    async fn recv_monitor_init_no_handle_awaits_reply() {
        let state: Option<Arc<SubscriptionState>> = None;
        let (tx, rx) = super::super::monitor_queue::MonitorSink::new(4, false);
        tx.push(dummy_monitor_frame());
        match recv_monitor_init(&state, &rx).await {
            MonitorInit::Reply(_) => {}
            _ => panic!("the no-handle path must still deliver the reply"),
        }
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
    fn put_get_init_success_decodes_both_introspections() {
        let frame = put_get_init_frame(ByteOrder::Little, crate::proto::Status::ok(), true);
        let mut cache = crate::pvdata::encode::TypeCache::new();
        match decode_put_get_init(&frame, &mut cache) {
            Ok(Ok((put_if, get_if))) => {
                assert!(matches!(put_if, FieldDesc::Structure { .. }));
                assert!(matches!(get_if, FieldDesc::Structure { .. }));
            }
            other => panic!("successful INIT must yield Ok(Ok((putIF, getIF))), got {other:?}"),
        }
    }

    fn scalar_string_struct() -> FieldDesc {
        FieldDesc::Structure {
            struct_id: "epics:nt/NTScalar:1.0".into(),
            fields: vec![("value".into(), FieldDesc::Scalar(ScalarType::String))],
        }
    }

    /// PUT_GET INIT with DISTINCT putIF (String value) and getIF (Int
    /// value): the decoder must return BOTH descriptors, not silently
    /// discard putIF. pvAccessCPP keeps `m_putData` (putIF) separate from
    /// `m_getData` (getIF); building the put leg or decoding a `getPut`
    /// against getIF would corrupt the wire layout when the structures
    /// differ (clientContextImpl.cpp:1036-1040).
    #[test]
    fn put_get_init_returns_distinct_put_and_get_descriptors() {
        let put = scalar_string_struct();
        let get = scalar_int_struct();
        let mut payload = Vec::new();
        payload.put_u32(7, ByteOrder::Little);
        payload.put_u8(QosFlags::INIT);
        crate::proto::Status::ok().write_into(ByteOrder::Little, &mut payload);
        encode_type_desc(&put, ByteOrder::Little, &mut payload); // putIF
        encode_type_desc(&get, ByteOrder::Little, &mut payload); // getIF
        let header = PvaHeader::application(
            true,
            ByteOrder::Little,
            Command::PutGet.code(),
            payload.len() as u32,
        );
        let frame = Frame { header, payload };
        let mut cache = crate::pvdata::encode::TypeCache::new();
        let (put_if, get_if) = match decode_put_get_init(&frame, &mut cache) {
            Ok(Ok(descs)) => descs,
            other => panic!("distinct INIT must decode to (putIF, getIF), got {other:?}"),
        };
        // putIF's value leaf is String; getIF's value leaf is Int — the two
        // must be carried separately.
        let put_value_ty = match &put_if {
            FieldDesc::Structure { fields, .. } => {
                fields.iter().find(|(n, _)| n == "value").map(|(_, d)| d)
            }
            _ => None,
        };
        let get_value_ty = match &get_if {
            FieldDesc::Structure { fields, .. } => {
                fields.iter().find(|(n, _)| n == "value").map(|(_, d)| d)
            }
            _ => None,
        };
        assert!(
            matches!(put_value_ty, Some(FieldDesc::Scalar(ScalarType::String))),
            "putIF value leaf must be String, got {put_value_ty:?}"
        );
        assert!(
            matches!(get_value_ty, Some(FieldDesc::Scalar(ScalarType::Int))),
            "getIF value leaf must be Int, got {get_value_ty:?}"
        );
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
    fn process_frame(
        order: ByteOrder,
        subcmd: u8,
        status: crate::proto::Status,
        full: bool,
    ) -> Frame {
        let mut payload = Vec::new();
        payload.put_u32(7, order);
        payload.put_u8(subcmd);
        if full {
            status.write_into(order, &mut payload);
        }
        let header =
            PvaHeader::application(true, order, Command::Process.code(), payload.len() as u32);
        Frame { header, payload }
    }

    #[test]
    fn process_non_success_status_is_per_op_not_fatal() {
        // Process-done phase (subcmd 0x00) with a non-success status is a
        // per-op result, not a connection fault.
        let frame = process_frame(
            ByteOrder::Little,
            0x00,
            crate::proto::Status::error("busy"),
            true,
        );
        match decode_process_status(&frame, false) {
            Ok(s) => assert!(
                !s.is_success(),
                "non-success PROCESS status is data, not Err"
            ),
            Err(e) => panic!("non-success PROCESS must decode to Ok(status), got Err({e:?})"),
        }
    }

    #[test]
    fn process_truncated_is_fatal_err() {
        let frame = process_frame(ByteOrder::Little, 0x00, crate::proto::Status::ok(), false);
        assert!(
            decode_process_status(&frame, false).is_err(),
            "a truncated PROCESS body must be a connection-fatal Err"
        );
    }

    #[test]
    fn process_wrong_command_is_fatal_err() {
        let mut frame = process_frame(ByteOrder::Little, 0x00, crate::proto::Status::ok(), true);
        frame.header.command = Command::Get.code();
        assert!(
            decode_process_status(&frame, false).is_err(),
            "a command mismatch must be a connection-fatal Err"
        );
    }

    #[test]
    fn process_init_phase_rejects_normal_subcmd() {
        // A normal (subcmd 0x00) reply to the INIT request is a phase
        // swap. pvAccess routes INIT vs done purely on QOS_INIT
        // (clientContextImpl.cpp:315): a normal response where INIT was
        // expected must be a connection-fatal Err, not a successful
        // create.
        let frame = process_frame(ByteOrder::Little, 0x00, crate::proto::Status::ok(), true);
        assert!(
            decode_process_status(&frame, true).is_err(),
            "a normal-phase reply where INIT was expected must be fatal"
        );
    }

    #[test]
    fn process_done_phase_rejects_init_subcmd() {
        // An INIT-bit (0x08) reply to the process request must not be
        // reported as process completion.
        let frame = process_frame(
            ByteOrder::Little,
            QosFlags::INIT,
            crate::proto::Status::ok(),
            true,
        );
        assert!(
            decode_process_status(&frame, false).is_err(),
            "an INIT-phase reply where process-done was expected must be fatal"
        );
    }

    #[test]
    fn process_init_phase_accepts_init_subcmd() {
        // The matching INIT
        // phase (subcmd 0x08) decodes to Ok(success).
        let frame = process_frame(
            ByteOrder::Little,
            QosFlags::INIT,
            crate::proto::Status::ok(),
            true,
        );
        match decode_process_status(&frame, true) {
            Ok(s) => assert!(
                s.is_success(),
                "matching INIT phase must be a success status"
            ),
            Err(e) => panic!("matching INIT phase must decode Ok, got Err({e:?})"),
        }
    }

    #[test]
    fn rpc_arg_null_encodes_single_null_type_tag() {
        // pvxs dataencode.cpp:30-35 — a null FieldDesc* is the single
        // 0xff byte; clientget.cpp:307-311 omits the value body. The
        // server's decode_rpc_exec_arg reads exactly this as a
        // top-level null argument.
        let mut out = Vec::new();
        encode_rpc_exec_arg(&RpcArg::Null, ByteOrder::Little, &mut out);
        assert_eq!(
            out,
            vec![0xff],
            "null RPC arg must be exactly the 0xff null-type tag with no value body"
        );
    }

    #[test]
    fn rpc_arg_typed_encodes_type_then_full_value() {
        let desc = FieldDesc::Scalar(ScalarType::Int);
        let value = PvField::Scalar(ScalarValue::Int(7));
        let mut got = Vec::new();
        encode_rpc_exec_arg(
            &RpcArg::Typed {
                desc: &desc,
                value: &value,
            },
            ByteOrder::Little,
            &mut got,
        );
        let mut want = Vec::new();
        encode_type_desc(&desc, ByteOrder::Little, &mut want);
        encode_pv_field(&value, &desc, ByteOrder::Little, &mut want);
        assert_eq!(
            got, want,
            "typed RPC arg must serialize as type(arg) + full_value(arg)"
        );
        assert_ne!(
            got,
            vec![0xff],
            "a typed arg must not collapse to the null tag"
        );
    }

    #[test]
    fn rpc_top_level_null_is_distinct_from_present_any_null() {
        // The finding's invariant: a top-level null argument (0xff) is
        // a different wire shape than a present `any` whose selected
        // value is null (variant tag 0x82, then the inner any-null
        // marker). Providers can tell the two apart.
        let mut null_arg = Vec::new();
        encode_rpc_exec_arg(&RpcArg::Null, ByteOrder::Little, &mut null_arg);

        let variant_desc = FieldDesc::Variant;
        let variant_null = build_put_value(&FieldDesc::Variant, "").unwrap();
        let mut any_arg = Vec::new();
        encode_rpc_exec_arg(
            &RpcArg::Typed {
                desc: &variant_desc,
                value: &variant_null,
            },
            ByteOrder::Little,
            &mut any_arg,
        );

        assert_eq!(null_arg, vec![0xff]);
        assert_ne!(
            any_arg, null_arg,
            "present any-null must not collapse to the top-level 0xff null tag"
        );
        assert_eq!(
            any_arg.first(),
            Some(&0x82),
            "a present any starts with the variant type tag, not 0xff"
        );
    }
}
